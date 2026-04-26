//! The drawer thread — single owner of all terminal output.
//!
//! The drawer consumes [`LifecycleEvent`]s from runtime threads and
//! [`PipeChunk`]s from the pipe reader threads, plus a redraw timer
//! and a shutdown signal. It maintains in-flight [`TestState`],
//! attributes captured bytes to tests via the `thread_to_test` table
//! populated from lifecycle events, and renders either:
//!
//! - **Live mode** ([`OutputMode::Live`]): a two-row-per-thread
//!   status region pinned to the bottom of the terminal. Completed
//!   tests' full blocks emit into the append-only history region
//!   above on every `TestCompleted`.
//! - **Plain mode** ([`OutputMode::Plain`]): linear append-only
//!   output — `started` line on `TestStarted`, `[name] line` for
//!   every captured line, final status line on `TestCompleted`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Result as IoResult, Write as _};
use std::mem;
use std::str;
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};

use super::color::ColorPolicy;
use super::events::{LifecycleEvent, PipeChunk, StdStream, TestId, TestState, TestStateKind};
use crate::bench::{BenchProgressSnapshot, HISTOGRAM_BUCKETS};
use crate::config::{Format, OutputMode};
use crate::runner::{normalize_module_path, qualified_test_name};
use crate::suite::{TeardownResult, TestOutcome};

const HINT_MAX_WIDTH: usize = 120;
/// Number of viewport rows reserved for everything other than a
/// single test's output stream — the status row itself, the runner's
/// own progress markers, and a safety margin so a final completion
/// line doesn't trigger a scroll while the live region is being
/// cleared.
const LIVE_REGION_RESERVED_ROWS: usize = 4;
/// The minimum gap between the left side and the right-aligned
/// trailing `<...>` block when the line would otherwise be shorter
/// than the terminal.
const MIN_TRAILING_PAD: usize = 2;
const REDRAW_INTERVAL: Duration = Duration::from_millis(50);
const RUNTIME_PREFIX_WIDTH: usize = 14;
/// Fixed visible width of the status-tag column (including trailing
/// pad so the rest of the line starts at a stable column).
pub const STATUS_TAG_WIDTH: usize = 9;

/// Type alias for the lifecycle channel Sender — used by runner
/// and macro-generated code that publishes events.
pub type LifecycleSender = Sender<LifecycleEvent>;

/// Persistent drawer state. Constructed and handed to
/// [`spawn_drawer`]; the main loop lives in [`Drawer::run`].
#[derive(Debug)]
pub struct Drawer {
    color: ColorPolicy,
    format: Format,
    last_live_rows: usize,
    lifecycle_rx: Receiver<LifecycleEvent>,
    output_mode: OutputMode,
    pipe_rx: Receiver<PipeChunk>,
    shutdown_rx: Receiver<()>,
    /// Test-only override for the terminal size queries. Production
    /// uses `None`, falling through to the ioctl-driven
    /// [`terminal_width`] / [`terminal_height`].
    size_override: Option<(usize, usize)>,
    slot_order: Vec<ThreadId>,
    slots: HashMap<ThreadId, RuntimeSlot>,
    summary: Summary,
    terminal: File,
    tests: HashMap<TestId, TestState>,
    thread_to_test: HashMap<ThreadId, TestId>,
}

#[derive(Debug)]
struct RuntimeSlot {
    current: Option<TestId>,
    /// Suite-lifecycle activity occupying this slot. While a suite's
    /// setup or teardown is in flight, no test runs on the slot's
    /// thread, so the live region renders this in place of the
    /// usual running-test row.
    lifecycle: Option<SlotLifecycle>,
    runtime_name: &'static str,
}

/// A suite-level operation currently occupying a runtime slot. The
/// drawer paints this in the live region with an elapsed counter
/// just like a running test.
#[derive(Debug, Clone, Copy)]
struct SlotLifecycle {
    kind: LifecyclePhase,
    started_at: Instant,
    suite: &'static str,
}

/// What went wrong in a suite-lifecycle finish event.
#[derive(Debug)]
enum LifecycleFailure {
    Error(String),
    /// Phase escalated past `--phase-hang-grace`. The wrapper fired
    /// the abort handle; on tokio the spawned task drops on next
    /// poll, on other runtimes it leaks until process exit.
    Hung(String),
    Panicked(String),
    TimedOut(String),
}

#[derive(Debug, Clone, Copy)]
enum LifecyclePhase {
    Setup,
    Teardown,
}

#[derive(Debug, Clone, Copy)]
enum StatusLabel {
    Bench,
    BenchErr,
    Cancel,
    Fail,
    /// Test or teardown blew its budget AND remained pending past the
    /// Layer-2 grace window. The wrapper has fired its abort handle
    /// and moved on. Painted **red** (failure-class) so it's distinct
    /// from `Timeout`'s yellow (warn-class).
    Hang,
    Ignore,
    Ok,
    Panic,
    Run,
    /// Failed test outcome where the per-test context (`Suite::context`)
    /// returned `Err` before the body could run. Distinct from `Fail`
    /// so the user sees that the test never executed.
    Setup,
    /// Suite-level setup completed successfully — bright variant of
    /// `Ok` reserved for lifecycle lines so they stand apart from the
    /// per-test stream.
    SetupOk,
    Timeout,
}

#[derive(Debug, Default)]
struct Summary {
    benched: usize,
    cancelled: usize,
    failed: usize,
    failures: Vec<FailureRecord>,
    /// Tests escalated past `--phase-hang-grace`. Counted separately
    /// so the summary line can show `N hung` distinct from `N timed
    /// out` and the renderer can paint a red `[HANG]` tag.
    hung: usize,
    ignored: usize,
    panicked: usize,
    passed: usize,
    started_at: Option<Instant>,
    teardown_failures: usize,
    timed_out: usize,
}

#[derive(Debug)]
struct FailureRecord {
    captured_stderr: String,
    captured_stdout: String,
    display_name: String,
    message: String,
    outcome_label: &'static str,
}

impl Drawer {
    fn clear_live_region(&mut self) {
        if self.last_live_rows == 0 {
            return;
        }
        let esc = format!("\x1b[{n}A\x1b[J", n = self.last_live_rows);
        let _unused = self.terminal.write_all(esc.as_bytes());
        self.last_live_rows = 0;
    }

    fn drain_remaining(&mut self) {
        while let Ok(ev) = self.lifecycle_rx.try_recv() {
            self.handle_lifecycle(ev);
        }
        while let Ok(chunk) = self.pipe_rx.try_recv() {
            self.handle_pipe(chunk);
        }
    }

    fn emit_completion_block(&mut self, state: &TestState, outcome: &TestOutcome) {
        if matches!(self.output_mode, OutputMode::Live) {
            self.clear_live_region();
        }
        let display = qualified_test_name(state.module_path, state.test_name);
        let label = status_label_from_outcome(outcome);
        let tag_rendered = render_status_tag(label, self.color);
        let trailing = trailing_info(outcome, state.runtime_name);
        let lhs_naked = format!("{:width$} {display}", "", width = STATUS_TAG_WIDTH);
        let lhs_rendered = format!("{tag_rendered} {display}");
        let term_cols = terminal_width();
        let header = render_line(&lhs_naked, &lhs_rendered, &trailing, term_cols);
        let _unused = self.terminal.write_all(header.as_bytes());
        let _unused = self.terminal.write_all(b"\n");

        // Surface the failure message right under the header so the
        // user sees WHY a test failed/setup-failed/etc. without
        // scrolling to the end-of-run failures section. One line per
        // newline in the message; each indented to line up under the
        // test display, painted in the tag's color.
        let msg = outcome_message(outcome);
        if !msg.is_empty() {
            for line in msg.lines() {
                let body = format!("  {line}\n");
                let painted = match label {
                    StatusLabel::Fail
                    | StatusLabel::Panic
                    | StatusLabel::Setup
                    | StatusLabel::Timeout
                    | StatusLabel::Cancel
                    | StatusLabel::Hang => self.color.red(&body),
                    _ => body,
                };
                let _unused = self.terminal.write_all(painted.as_bytes());
            }
        }

        // In plain mode we already printed bytes live; in live mode
        // this is the first time captured output hits the terminal.
        if matches!(self.output_mode, OutputMode::Live) {
            emit_captured_block(&mut self.terminal, state, self.color);
        }

        if let TestOutcome::Benched { report, .. } = outcome {
            let detail = report.detailed_summary();
            let _unused = self.terminal.write_all(detail.as_bytes());
            if report.failures.is_empty() && report.panics == 0 {
                let hist = report.ascii_histogram(10, 30);
                if !hist.is_empty() {
                    let _unused = self.terminal.write_all(b"  histogram:\n");
                    let _unused = self.terminal.write_all(hist.as_bytes());
                }
            }
        }
        let _unused = self.terminal.write_all(b"\n");
        self.last_live_rows = 0;
    }

    fn emit_plain_started(&mut self, test_id: TestId) {
        if let Some(state) = self.tests.get(&test_id) {
            let line = format!(
                "test {} ... started [{}]\n",
                qualified_test_name(state.module_path, state.test_name),
                state.runtime_name,
            );
            let _unused = self.terminal.write_all(line.as_bytes());
        }
    }

    fn handle_lifecycle(&mut self, ev: LifecycleEvent) {
        match ev {
            LifecycleEvent::TestStarted {
                test_id,
                module_path,
                test_name,
                runtime_name,
                thread,
                at,
            } => {
                let state = TestState {
                    module_path,
                    test_name,
                    runtime_name,
                    thread,
                    started_at: at,
                    kind: TestStateKind::Running,
                    stdout_buffer: Vec::new(),
                    stderr_buffer: Vec::new(),
                    last_output_line: String::new(),
                    recent_output: Vec::new(),
                };
                let _unused = self.tests.insert(test_id, state);
                let _unused = self.thread_to_test.insert(thread, test_id);
                let had_slot = self.slots.contains_key(&thread);
                if !had_slot {
                    self.slot_order.push(thread);
                }
                let entry = self.slots.entry(thread).or_insert(RuntimeSlot {
                    runtime_name,
                    current: None,
                    lifecycle: None,
                });
                entry.runtime_name = runtime_name;
                entry.current = Some(test_id);
                if matches!(self.output_mode, OutputMode::Plain) {
                    self.emit_plain_started(test_id);
                }
            }
            LifecycleEvent::BenchProgress { test_id, snapshot } => {
                if let Some(state) = self.tests.get_mut(&test_id) {
                    state.kind = TestStateKind::Bench { snapshot };
                }
            }
            LifecycleEvent::TestIgnored {
                module_path,
                test_name,
                runtime_name,
                reason,
            } => {
                self.summary.ignored = self.summary.ignored.saturating_add(1);
                let display = qualified_test_name(module_path, test_name);
                if matches!(self.output_mode, OutputMode::Live) {
                    self.clear_live_region();
                }
                let tag_rendered = render_status_tag(StatusLabel::Ignore, self.color);
                let lhs_naked = format!("{:width$} {display}", "", width = STATUS_TAG_WIDTH);
                let lhs_rendered = format!("{tag_rendered} {display}");
                let trailing = if reason.is_empty() {
                    format!("<{runtime_name}>")
                } else {
                    format!("<{runtime_name}, {reason}>")
                };
                let line = render_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                let _unused = self.terminal.write_all(line.as_bytes());
                let _unused = self.terminal.write_all(b"\n");
                self.last_live_rows = 0;
            }
            LifecycleEvent::SuiteSetupStarted {
                runtime_name,
                suite,
                thread,
                at,
            } => {
                self.handle_suite_lifecycle_start(
                    LifecyclePhase::Setup,
                    runtime_name,
                    suite,
                    thread,
                    at,
                );
            }
            LifecycleEvent::SuiteSetupFinished {
                runtime_name,
                suite,
                thread,
                elapsed,
                error,
            } => {
                self.handle_suite_lifecycle_finish(
                    LifecyclePhase::Setup,
                    runtime_name,
                    suite,
                    thread,
                    elapsed,
                    error.map(LifecycleFailure::Error),
                );
            }
            LifecycleEvent::SuiteTeardownStarted {
                runtime_name,
                suite,
                thread,
                at,
            } => {
                self.handle_suite_lifecycle_start(
                    LifecyclePhase::Teardown,
                    runtime_name,
                    suite,
                    thread,
                    at,
                );
            }
            LifecycleEvent::SuiteTeardownFinished {
                runtime_name,
                suite,
                thread,
                elapsed,
                result,
            } => {
                let failure = match result {
                    TeardownResult::Ok => None,
                    TeardownResult::Err(msg) => Some(LifecycleFailure::Error(msg)),
                    TeardownResult::Panicked(msg) => Some(LifecycleFailure::Panicked(msg)),
                    TeardownResult::TimedOut => {
                        Some(LifecycleFailure::TimedOut("teardown timed out".to_owned()))
                    }
                    TeardownResult::Hung => Some(LifecycleFailure::Hung(
                        "teardown hung; abort signal sent".to_owned(),
                    )),
                };
                if failure.is_some() {
                    self.summary.teardown_failures =
                        self.summary.teardown_failures.saturating_add(1);
                }
                self.handle_suite_lifecycle_finish(
                    LifecyclePhase::Teardown,
                    runtime_name,
                    suite,
                    thread,
                    elapsed,
                    failure,
                );
            }
            LifecycleEvent::TestTeardownFailed {
                module_path,
                test_name,
                runtime_name,
                result,
            } => {
                if matches!(self.output_mode, OutputMode::Live) {
                    self.clear_live_region();
                }
                let display = qualified_test_name(module_path, test_name);
                let (label, label_text, message) = match result {
                    TeardownResult::Ok => return,
                    TeardownResult::Err(msg) => (StatusLabel::Fail, "error", msg),
                    TeardownResult::Panicked(msg) => (StatusLabel::Panic, "panic", msg),
                    TeardownResult::TimedOut => (
                        StatusLabel::Timeout,
                        "timeout",
                        "teardown timed out".to_owned(),
                    ),
                    TeardownResult::Hung => (
                        StatusLabel::Hang,
                        "hang",
                        "teardown hung; abort signal sent".to_owned(),
                    ),
                };
                let tag_rendered = render_status_tag(label, self.color);
                let lhs_display = format!("teardown {display}");
                let trailing = format!("<{runtime_name}>");
                let lhs_naked = format!("{:width$} {lhs_display}", "", width = STATUS_TAG_WIDTH);
                let lhs_rendered = format!("{tag_rendered} {lhs_display}");
                let header = render_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                let _unused = self.terminal.write_all(header.as_bytes());
                let _unused = self.terminal.write_all(b"\n");
                let body = format!("  {label_text}: {message}\n");
                let painted = self.color.red(&body);
                let _unused = self.terminal.write_all(painted.as_bytes());
                self.summary.teardown_failures = self.summary.teardown_failures.saturating_add(1);
                self.summary.failures.push(FailureRecord {
                    display_name: format!("teardown {display}"),
                    outcome_label: "TEST TEARDOWN FAILED",
                    message: format!("{label_text}: {message}"),
                    captured_stderr: String::new(),
                    captured_stdout: String::new(),
                });
                self.last_live_rows = 0;
            }
            LifecycleEvent::TestCompleted { test_id, outcome } => {
                // Drain the pipe aggressively so any bytes the test
                // wrote just before the runtime thread flushed land
                // in the correct test's buffer.
                while let Ok(chunk) = self.pipe_rx.try_recv() {
                    self.handle_pipe(chunk);
                }
                self.summary.record_outcome(&outcome);
                if let Some(state) = self.tests.remove(&test_id) {
                    if is_failure(&outcome) {
                        self.summary.failures.push(FailureRecord {
                            display_name: qualified_test_name(state.module_path, state.test_name),
                            outcome_label: outcome_label(&outcome),
                            message: outcome_message(&outcome),
                            captured_stderr: String::from_utf8_lossy(&state.stderr_buffer)
                                .into_owned(),
                            captured_stdout: String::from_utf8_lossy(&state.stdout_buffer)
                                .into_owned(),
                        });
                    }
                    self.emit_completion_block(&state, &outcome);
                    // Clear the thread and slot mappings *after*
                    // emission so late-drained bytes attributed via
                    // the thread map are already flushed.
                    let _unused = self.thread_to_test.remove(&state.thread);
                    if let Some(slot) = self.slots.get_mut(&state.thread)
                        && slot.current == Some(test_id) {
                            slot.current = None;
                        }
                }
            }
        }
    }

    fn handle_pipe(&mut self, chunk: PipeChunk) {
        // Attribution: the reader thread has no producer-thread info.
        // When exactly one test is running, attribute to it. When
        // multiple are running concurrently, attribute to the
        // earliest-started (deterministic fallback — doc'd as
        // best-effort for concurrent output).
        let chosen: Option<TestId> = if self.tests.len() == 1 {
            self.tests.keys().copied().next()
        } else {
            self.thread_to_test
                .values()
                .copied()
                .min_by_key(|id| self.tests.get(id).map(|state| state.started_at))
        };
        if let Some(id) = chosen
            && let Some(state) = self.tests.get_mut(&id) {
                match chunk.stream {
                    StdStream::Stdout => state.stdout_buffer.extend_from_slice(&chunk.bytes),
                    StdStream::Stderr => state.stderr_buffer.extend_from_slice(&chunk.bytes),
                }
                update_last_line(&mut state.last_output_line, &chunk.bytes);
                append_complete_lines(&mut state.recent_output, &chunk.bytes);
                if matches!(self.output_mode, OutputMode::Plain) {
                    let display = qualified_test_name(state.module_path, state.test_name);
                    emit_plain_lines(
                        &mut self.terminal,
                        &chunk.bytes,
                        &display,
                        chunk.stream,
                        self.color,
                    );
                }
                return;
            }
        // Orphan bytes (no mapped test): pass through unprefixed.
        // Live mode: clear live region first; next redraw re-paints.
        if matches!(self.output_mode, OutputMode::Live) {
            self.clear_live_region();
        }
        let _unused = self.terminal.write_all(&chunk.bytes);
    }

    fn handle_suite_lifecycle_finish(
        &mut self,
        kind: LifecyclePhase,
        runtime_name: &'static str,
        suite: &'static str,
        thread: ThreadId,
        elapsed: Duration,
        failure: Option<LifecycleFailure>,
    ) {
        if let Some(slot) = self.slots.get_mut(&thread) {
            slot.lifecycle = None;
        }
        if matches!(self.output_mode, OutputMode::Live) {
            self.clear_live_region();
        }
        let phase_word = match kind {
            LifecyclePhase::Setup => "setup",
            LifecyclePhase::Teardown => "teardown",
        };
        let label = match (kind, &failure) {
            (LifecyclePhase::Setup | LifecyclePhase::Teardown,
Some(LifecycleFailure::TimedOut(_))) => {
                StatusLabel::Timeout
            }
            (LifecyclePhase::Setup | LifecyclePhase::Teardown,
Some(LifecycleFailure::Hung(_))) => StatusLabel::Hang,
            (LifecyclePhase::Setup, None) => StatusLabel::SetupOk,
            // Suite-level setup failure renders as [FAIL]; the
            // [SETUP] tag is reserved for per-test SetupFailed.
            (LifecyclePhase::Setup, Some(LifecycleFailure::Error(_))) => StatusLabel::Fail,
            (LifecyclePhase::Setup, Some(LifecycleFailure::Panicked(_))) => StatusLabel::Panic,
            (LifecyclePhase::Teardown, None) => StatusLabel::Ok,
            (LifecyclePhase::Teardown, Some(LifecycleFailure::Error(_))) => StatusLabel::Fail,
            (LifecyclePhase::Teardown, Some(LifecycleFailure::Panicked(_))) => StatusLabel::Panic,
        };
        let tag_rendered = render_status_tag(label, self.color);
        let suite_disp = normalize_module_path(suite);
        let display = format!("{phase_word} {suite_disp}");
        let trailing = format!("<{runtime_name}, {elapsed:.2?}>");
        let lhs_naked = format!("{:width$} {display}", "", width = STATUS_TAG_WIDTH);
        let lhs_rendered = format!("{tag_rendered} {display}");
        let header = render_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
        let _unused = self.terminal.write_all(header.as_bytes());
        let _unused = self.terminal.write_all(b"\n");

        if let Some(failure) = failure {
            let (label_text, message) = match failure {
                LifecycleFailure::Error(msg) => ("error", msg),
                LifecycleFailure::Panicked(msg) => ("panic", msg),
                LifecycleFailure::TimedOut(msg) => ("timeout", msg),
                LifecycleFailure::Hung(msg) => ("hang", msg),
            };
            let body = format!("  {label_text}: {message}\n");
            let painted = self.color.red(&body);
            let _unused = self.terminal.write_all(painted.as_bytes());
            self.summary.failures.push(FailureRecord {
                display_name: format!("{phase_word} {suite_disp}"),
                outcome_label: match kind {
                    LifecyclePhase::Setup => "SUITE SETUP FAILED",
                    LifecyclePhase::Teardown => "SUITE TEARDOWN FAILED",
                },
                message: format!("{label_text}: {message}"),
                captured_stderr: String::new(),
                captured_stdout: String::new(),
            });
        }
        self.last_live_rows = 0;
    }

    fn handle_suite_lifecycle_start(
        &mut self,
        kind: LifecyclePhase,
        runtime_name: &'static str,
        suite: &'static str,
        thread: ThreadId,
        at: Instant,
    ) {
        if !self.slots.contains_key(&thread) {
            self.slot_order.push(thread);
        }
        let entry = self.slots.entry(thread).or_insert(RuntimeSlot {
            runtime_name,
            current: None,
            lifecycle: None,
        });
        entry.runtime_name = runtime_name;
        entry.lifecycle = Some(SlotLifecycle {
            kind,
            suite,
            started_at: at,
        });
        if matches!(self.output_mode, OutputMode::Plain) {
            let phase_word = match kind {
                LifecyclePhase::Setup => "setup",
                LifecyclePhase::Teardown => "teardown",
            };
            let suite_disp = normalize_module_path(suite);
            let line = format!("{phase_word:<8} {suite_disp} ... started <{runtime_name}>\n");
            let _unused = self.terminal.write_all(line.as_bytes());
        }
    }

    /// Build a drawer. Slots are allocated lazily as `TestStarted`
    /// events come in — one per distinct `ThreadId`, in first-seen
    /// order — so the runner doesn't have to know runtime names
    /// up-front.
    #[must_use]
    #[inline]
    pub fn new(
        lifecycle_rx: Receiver<LifecycleEvent>,
        pipe_rx: Receiver<PipeChunk>,
        shutdown_rx: Receiver<()>,
        terminal: File,
        output_mode: OutputMode,
        format: Format,
        color: ColorPolicy,
    ) -> Self {
        Self {
            lifecycle_rx,
            pipe_rx,
            shutdown_rx,
            terminal,
            output_mode,
            format,
            color,
            slot_order: Vec::new(),
            slots: HashMap::new(),
            tests: HashMap::new(),
            thread_to_test: HashMap::new(),
            last_live_rows: 0,
            summary: Summary::default(),
            size_override: None,
        }
    }

    fn print_final_summary(&mut self) {
        let total_elapsed = self
            .summary
            .started_at
            .map_or(Duration::ZERO, |start| start.elapsed());
        if !self.summary.failures.is_empty() {
            let _unused = self.terminal.write_all(b"\nfailures:\n\n");
            for fr in &self.summary.failures {
                let header = format!("---- {} {} ----\n", fr.display_name, fr.outcome_label);
                let _unused = self.terminal.write_all(header.as_bytes());
                if !fr.message.is_empty() {
                    let _unused = self.terminal.write_all(fr.message.as_bytes());
                    if !fr.message.ends_with('\n') {
                        let _unused = self.terminal.write_all(b"\n");
                    }
                }
                if !fr.captured_stdout.is_empty() {
                    let _unused = self.terminal.write_all(b"---- stdout ----\n");
                    let _unused = self.terminal.write_all(fr.captured_stdout.as_bytes());
                    if !fr.captured_stdout.ends_with('\n') {
                        let _unused = self.terminal.write_all(b"\n");
                    }
                }
                if !fr.captured_stderr.is_empty() {
                    let _unused = self.terminal.write_all(b"---- stderr ----\n");
                    let coloured = self.color.red(&fr.captured_stderr);
                    let _unused = self.terminal.write_all(coloured.as_bytes());
                    if !fr.captured_stderr.ends_with('\n') {
                        let _unused = self.terminal.write_all(b"\n");
                    }
                }
                let _unused = self.terminal.write_all(b"\n");
            }
        }
        let failed_total = self.summary.failed
            + self.summary.panicked
            + self.summary.timed_out
            + self.summary.hung
            + self.summary.cancelled
            + self.summary.teardown_failures;
        let overall = if failed_total == 0 {
            self.color.green("ok")
        } else {
            self.color.red("FAILED")
        };
        let summary_line = format!(
            "\ntest result: {overall}. {} passed; {failed_total} failed; \
             {} benched; {} hung; {} ignored; {} teardown failed; finished in {:.2}s\n",
            self.summary.passed,
            self.summary.benched,
            self.summary.hung,
            self.summary.ignored,
            self.summary.teardown_failures,
            total_elapsed.as_secs_f64(),
        );
        let _unused = self.terminal.write_all(summary_line.as_bytes());
        let _unused = self.terminal.flush();
    }

    fn redraw_live_region(&mut self) {
        if !matches!(self.output_mode, OutputMode::Live) || matches!(self.format, Format::Terse) {
            return;
        }
        // Build the live region from slots that have *actual* activity
        // — an in-flight test or a suite setup/teardown. Idle slots
        // contribute no row. When every slot is idle the live region
        // collapses to nothing on the next clear, instead of stamping
        // a `── running ──` header + `[IDLE]` rows on every 50ms tick
        // — which reads as "we're announcing 'running' while nothing
        // is running" and bloats the scrollback once the user pages
        // back through it.
        let mut buf = String::new();
        let mut rows = 0_usize;
        for thread in &self.slot_order {
            let Some(slot) = self.slots.get(thread) else {
                continue;
            };
            let (cols, rows_cap) = self.render_size();
            let (status_line, output_lines) = if let Some(lifecycle) = slot.lifecycle {
                (
                    lifecycle_line(slot.runtime_name, &lifecycle, self.color, cols),
                    Vec::new(),
                )
            } else if let Some(state) = slot.current.and_then(|id| self.tests.get(&id)) {
                let mut output_lines = running_output_lines(state, self.color, cols, rows_cap);
                if let TestStateKind::Bench { snapshot } = state.kind {
                    let used = output_lines.len().saturating_add(1);
                    let remaining = rows_cap
                        .saturating_sub(LIVE_REGION_RESERVED_ROWS)
                        .saturating_sub(used);
                    output_lines.extend(bench_histogram_lines(
                        &snapshot, self.color, cols, remaining,
                    ));
                }
                (
                    running_line(slot.runtime_name, state, self.color, cols),
                    output_lines,
                )
            } else {
                continue;
            };
            buf.push_str(&status_line);
            buf.push('\n');
            rows = rows.saturating_add(1);
            // Stream the test's stdout/stderr live below the status
            // row, untruncated, in source order.
            for line in &output_lines {
                buf.push_str(line);
                buf.push('\n');
                rows = rows.saturating_add(1);
            }
        }
        // Clear last frame regardless of whether we have something new
        // to paint — this is what makes the region collapse cleanly
        // when the active-slot count drops to zero.
        self.clear_live_region();
        if rows == 0 {
            return;
        }
        let _unused = self.terminal.write_all(buf.as_bytes());
        let _unused = self.terminal.flush();
        self.last_live_rows = rows;
    }

    fn render_size(&self) -> (usize, usize) {
        self.size_override
            .unwrap_or_else(|| (terminal_width(), terminal_height()))
    }

    /// Main loop: `select!` over all input channels plus a redraw
    /// timer until shutdown. On exit, drain pending events, clear
    /// the live region, and print the final summary.
    #[inline]
    pub fn run(mut self) {
        self.summary.started_at = Some(Instant::now());
        let timer = crossbeam_channel::tick(REDRAW_INTERVAL);
        loop {
            select! {
                recv(self.lifecycle_rx) -> msg => match msg {
                    Ok(ev) => self.handle_lifecycle(ev),
                    Err(_) => break,
                },
                recv(self.pipe_rx) -> msg => match msg {
                    Ok(chunk) => self.handle_pipe(chunk),
                    Err(_) => break,
                },
                recv(timer) -> _tick => self.redraw_live_region(),
                recv(self.shutdown_rx) -> _done => break,
            }
        }
        self.drain_remaining();
        self.clear_live_region();
        self.print_final_summary();
    }

    /// Force the drawer to render against a fixed terminal size,
    /// bypassing the ioctl probe. Hidden from the public API surface
    /// — only the in-tree integration tests use this to pin the
    /// "row never wraps" invariant against synthetic widths.
    #[doc(hidden)]
    #[must_use]
    #[inline]
    pub const fn with_size_override(mut self, cols: usize, height: usize) -> Self {
        self.size_override = Some((cols, height));
        self
    }
}

impl Summary {
    const fn record_outcome(&mut self, outcome: &TestOutcome) {
        match outcome {
            TestOutcome::Passed { .. } => self.passed = self.passed.saturating_add(1),
            TestOutcome::Failed { .. } | TestOutcome::SetupFailed { .. } => {
                self.failed = self.failed.saturating_add(1);
            }
            TestOutcome::Panicked { .. } => self.panicked = self.panicked.saturating_add(1),
            TestOutcome::TimedOut => self.timed_out = self.timed_out.saturating_add(1),
            TestOutcome::Hung { .. } => self.hung = self.hung.saturating_add(1),
            TestOutcome::Cancelled => self.cancelled = self.cancelled.saturating_add(1),
            TestOutcome::Benched { report, .. } => {
                self.benched = self.benched.saturating_add(1);
                if !report.is_success() {
                    self.failed = self.failed.saturating_add(1);
                }
            }
        }
    }
}

const fn is_failure(outcome: &TestOutcome) -> bool {
    match outcome {
        TestOutcome::Failed { .. }
        | TestOutcome::Panicked { .. }
        | TestOutcome::SetupFailed { .. }
        | TestOutcome::TimedOut
        | TestOutcome::Hung { .. }
        | TestOutcome::Cancelled => true,
        TestOutcome::Benched { report, .. } => !report.is_success(),
        TestOutcome::Passed { .. } => false,
    }
}

const fn status_label_from_outcome(outcome: &TestOutcome) -> StatusLabel {
    match outcome {
        TestOutcome::Passed { .. } => StatusLabel::Ok,
        TestOutcome::Failed { .. } => StatusLabel::Fail,
        TestOutcome::Panicked { .. } => StatusLabel::Panic,
        TestOutcome::SetupFailed { .. } => StatusLabel::Setup,
        TestOutcome::TimedOut => StatusLabel::Timeout,
        TestOutcome::Hung { .. } => StatusLabel::Hang,
        TestOutcome::Cancelled => StatusLabel::Cancel,
        TestOutcome::Benched { report, .. } => {
            if report.failures.is_empty() && report.panics == 0 {
                StatusLabel::Bench
            } else {
                StatusLabel::BenchErr
            }
        }
    }
}

/// Produce the coloured, padded status tag — pad to
/// [`STATUS_TAG_WIDTH`] characters visible width so subsequent
/// columns line up across every status kind.
fn render_status_tag(label: StatusLabel, color: ColorPolicy) -> String {
    let word = match label {
        StatusLabel::Ok | StatusLabel::SetupOk => "OK",
        StatusLabel::Fail => "FAIL",
        StatusLabel::Panic => "PANIC",
        StatusLabel::Timeout => "TIMEOUT",
        StatusLabel::Ignore => "IGNORE",
        StatusLabel::Cancel => "CANCEL",
        StatusLabel::Bench | StatusLabel::BenchErr => "BENCH",
        StatusLabel::Run => "RUN",
        StatusLabel::Setup => "SETUP",
        StatusLabel::Hang => "HANG",
    };
    let naked = format!("[{word}]");
    let visible = naked.chars().count();
    let painted = match label {
        StatusLabel::Ok | StatusLabel::Bench | StatusLabel::SetupOk => color.green(&naked),
        StatusLabel::Fail
        | StatusLabel::Panic
        | StatusLabel::BenchErr
        | StatusLabel::Setup
        | StatusLabel::Hang => color.red(&naked),
        StatusLabel::Timeout | StatusLabel::Cancel | StatusLabel::Run => color.yellow(&naked),
        StatusLabel::Ignore => color.dim(&naked),
    };
    let pad = STATUS_TAG_WIDTH.saturating_sub(visible);
    let mut out = painted;
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

/// Build the trailing `<runtime, elapsed[, bench info]>` block for a
/// finished test.
fn trailing_info(outcome: &TestOutcome, runtime_name: &str) -> String {
    match outcome {
        TestOutcome::Passed { elapsed }
        | TestOutcome::Failed { elapsed, .. }
        | TestOutcome::Panicked { elapsed }
        | TestOutcome::Hung { elapsed }
        | TestOutcome::SetupFailed { elapsed, .. } => {
            format!("<{runtime_name}, {elapsed:.2?}>")
        }
        TestOutcome::TimedOut | TestOutcome::Cancelled => format!("<{runtime_name}>"),
        TestOutcome::Benched { elapsed, report } => {
            let median = report
                .median()
                .map(|median| format!(", p50 {median:.2?}"))
                .unwrap_or_default();
            format!(
                "<{runtime_name}, {elapsed:.2?}, {}{median}>",
                report.strategy,
            )
        }
    }
}

/// Right-align `trailing` to `term_cols` with at least
/// [`MIN_TRAILING_PAD`] spaces between the left side and it. When
/// `lhs + trailing` already overflow the line, falls back to the
/// minimum pad (line overflows cleanly to the right).
fn render_line(lhs_naked: &str, lhs_rendered: &str, trailing: &str, term_cols: usize) -> String {
    let lhs_visible = lhs_naked.chars().count();
    let trailing_visible = trailing.chars().count();
    let pad = term_cols
        .saturating_sub(lhs_visible)
        .saturating_sub(trailing_visible)
        .max(MIN_TRAILING_PAD);
    let mut out = String::with_capacity(lhs_rendered.len() + pad + trailing.len());
    out.push_str(lhs_rendered);
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(trailing);
    out
}

/// Best-effort terminal width for right-aligning. Uses `TIOCGWINSZ`
/// on Unix against the saved-original stdout (which the drawer's
/// terminal File wraps), falling back to 100 columns.
#[cfg(unix)]
fn terminal_width() -> usize {
    let (cols, _rows) = terminal_size_unix();
    cols.unwrap_or(100)
}

/// Best-effort terminal height. Used to bound the live region so it
/// never grows taller than the viewport — anything that overflows
/// scrolls permanently into scrollback and the cursor-up clear can't
/// erase it. Falls back to 24 rows when the ioctl can't reach a
/// terminal FD.
#[cfg(unix)]
fn terminal_height() -> usize {
    let (_cols, rows) = terminal_size_unix();
    rows.unwrap_or(24)
}

#[cfg(unix)]
fn terminal_size_unix() -> (Option<usize>, Option<usize>) {
    // SAFETY: ioctl TIOCGWINSZ writes a `winsize` we allocated; we
    // only read it on success. The drawer doesn't redirect FD 2
    // after the capture swap (FD 2 points at the write end of the
    // stderr capture pipe, so it's not useful) — we walk likely-
    // terminal FDs until one answers.
    #[expect(unsafe_code)]
    unsafe {
        let mut ws: libc::winsize = mem::zeroed();
        for fd in [libc::STDERR_FILENO, libc::STDIN_FILENO, libc::STDOUT_FILENO] {
            if libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut ws) == 0 && ws.ws_col > 0 {
                let cols = Some(usize::from(ws.ws_col));
                let rows = (ws.ws_row > 0).then(|| usize::from(ws.ws_row));
                return (cols, rows);
            }
        }
    }
    (None, None)
}

#[cfg(not(unix))]
fn terminal_width() -> usize {
    100
}

#[cfg(not(unix))]
fn terminal_height() -> usize {
    24
}

const fn outcome_label(outcome: &TestOutcome) -> &'static str {
    match outcome {
        TestOutcome::Failed { .. } => "FAILED",
        TestOutcome::Panicked { .. } => "PANICKED",
        TestOutcome::SetupFailed { .. } => "SETUP FAILED",
        TestOutcome::TimedOut => "TIMED OUT",
        TestOutcome::Hung { .. } => "HUNG",
        TestOutcome::Cancelled => "CANCELLED",
        TestOutcome::Benched { .. } => "bench had errors",
        TestOutcome::Passed { .. } => "",
    }
}

fn outcome_message(outcome: &TestOutcome) -> String {
    match outcome {
        TestOutcome::Failed { message, .. } => message.clone(),
        TestOutcome::SetupFailed { message, .. } => {
            format!("test setup failed: {message}")
        }
        TestOutcome::TimedOut => "test exceeded its timeout".to_owned(),
        TestOutcome::Hung { .. } => "hung; abort signal sent".to_owned(),
        TestOutcome::Cancelled => "test was cancelled before completion".to_owned(),
        TestOutcome::Panicked { .. } | TestOutcome::Passed { .. } | TestOutcome::Benched { .. } => {
            String::new()
        }
    }
}

fn emit_captured_block(terminal: &mut File, state: &TestState, color: ColorPolicy) {
    for line in split_lines(&state.stdout_buffer) {
        let formatted = format!("  {line}\n");
        let _unused = terminal.write_all(formatted.as_bytes());
    }
    for line in split_lines(&state.stderr_buffer) {
        let formatted = format!("  {line}\n");
        let _unused = terminal.write_all(color.red(&formatted).as_bytes());
    }
}

fn emit_plain_lines(
    terminal: &mut File,
    bytes: &[u8],
    _display: &str,
    stream: StdStream,
    color: ColorPolicy,
) {
    for line in split_lines(bytes) {
        let formatted = format!("  {line}\n");
        let styled = match stream {
            StdStream::Stdout => formatted,
            StdStream::Stderr => color.red(&formatted),
        };
        let _unused = terminal.write_all(styled.as_bytes());
    }
}

fn split_lines(bytes: &[u8]) -> impl Iterator<Item = &str> {
    str::from_utf8(bytes)
        .unwrap_or("")
        .split('\n')
        .filter(|line| !line.is_empty())
}

fn update_last_line(dst: &mut String, bytes: &[u8]) {
    let Ok(text) = str::from_utf8(bytes) else {
        return;
    };
    for line in text.split('\n') {
        if !line.is_empty() {
            dst.clear();
            let truncated = if line.len() > HINT_MAX_WIDTH {
                &line[..HINT_MAX_WIDTH]
            } else {
                line
            };
            dst.push_str(truncated);
        }
    }
}

/// Append every complete, non-empty line in `bytes` to `dst`, oldest
/// first. Used to maintain `TestState::recent_output` for the live
/// streaming of test stdio under the running status row.
fn append_complete_lines(dst: &mut Vec<String>, bytes: &[u8]) {
    let Ok(text) = str::from_utf8(bytes) else {
        return;
    };
    for line in text.split('\n') {
        if !line.is_empty() {
            dst.push(line.to_owned());
        }
    }
}

/// Render a horizontal progress bar of `width` characters showing
/// `done / total` filled. Solid block `█` for filled cells, light shade
/// `░` for empty. Used by [`bench_progress_trailing`] for the inline
/// trailing-block bar.
fn bar_render(done: usize, total: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = if total == 0 {
        0
    } else {
        (done.saturating_mul(width) / total).min(width)
    };
    let mut out = String::with_capacity(width.saturating_add(2));
    out.push('[');
    for _ in 0..filled {
        out.push('\u{2588}');
    }
    for _ in filled..width {
        out.push('\u{2591}');
    }
    out.push(']');
    out
}

/// Build the trailing `<…>` block that follows a `[BENCH]` running
/// row. The block adapts to the available terminal width so a narrow
/// terminal still shows a useful summary instead of letting the row
/// wrap (which would corrupt the live-region geometry).
///
/// Width thresholds (the `cols` here is the full terminal width, so
/// the trailing budget is reduced internally by the runtime+tag
/// prefix overhead):
/// - `cols ≥ 100`: progress bar + percent + done/total + p50 + p95 + cov
/// - `cols ≥  80`: progress bar + percent + done/total + p50 + p95
/// - `cols ≥  60`: percent + done/total + p50
/// - `cols ≥  50`: percent + done/total
/// - else:         percent only (`<42%>`)
///
/// `cov` is silently dropped when non-finite (n < 2 or mean = 0).
#[doc(hidden)]
#[must_use]
#[inline]
pub fn bench_progress_trailing(
    snap: &BenchProgressSnapshot,
    cols: usize,
    _elapsed: Duration,
) -> String {
    let pct = if snap.total == 0 {
        0
    } else {
        snap.done.saturating_mul(100) / snap.total
    };
    let bar = bar_render(snap.done, snap.total, 10);
    let cov_finite = snap.cov.is_finite();
    if cols >= 100 && cov_finite {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "cov rendering; precision loss well within display rounding."
        )]
        {
            let cov_pct = (f64::from(snap.cov) * 100.0_f64) as f32;
            return format!(
                "<{bar} {pct}% {}/{}  p50={p50:.0?}  p95={p95:.0?}  cov={cov_pct:.1}%>",
                snap.done,
                snap.total,
                p50 = snap.p50,
                p95 = snap.p95,
            );
        }
    }
    if cols >= 80 {
        return format!(
            "<{bar} {pct}% {}/{} p50={p50:.0?} p95={p95:.0?}>",
            snap.done,
            snap.total,
            p50 = snap.p50,
            p95 = snap.p95,
        );
    }
    if cols >= 60 {
        return format!(
            "<{pct}% {}/{} p50={p50:.0?}>",
            snap.done,
            snap.total,
            p50 = snap.p50,
        );
    }
    if cols >= 50 {
        return format!("<{pct}% {}/{}>", snap.done, snap.total);
    }
    format!("<{pct}%>")
}

/// Build the live-region status row for a running test. Public for
/// in-tree integration tests that want to pin the "row never wraps"
/// invariant against synthetic widths; production callers go through
/// the `Drawer` which queries the terminal via [`terminal_width`].
#[doc(hidden)]
#[must_use]
#[inline]
pub fn running_line(runtime: &str, state: &TestState, color: ColorPolicy, cols: usize) -> String {
    let prefix = format!("{runtime:<RUNTIME_PREFIX_WIDTH$}");
    let label = match state.kind {
        TestStateKind::Running => StatusLabel::Run,
        TestStateKind::Bench { .. } => StatusLabel::Bench,
    };
    let tag = render_status_tag(label, color);
    let elapsed = state.started_at.elapsed();
    let trailing = match state.kind {
        TestStateKind::Running => format!("<{elapsed:.2?}>"),
        TestStateKind::Bench { snapshot } => bench_progress_trailing(&snapshot, cols, elapsed),
    };
    // Clip `display` so the rendered row stays *strictly* inside
    // `cols` (we leave 1 col slack at the right edge): a row that
    // ends exactly at `cols` chars can still wrap on terminals with
    // DECAWM (auto-wrap) on, because the cursor sits "in" the right
    // margin and the implicit newline lands one row down. The
    // tracked `last_live_rows` would then undercount by 1 and the
    // cursor-up clear would strand the wrap-overflow row in the
    // user's scrollback as a stale `[RUN]` stripe.
    let display = {
        let raw = qualified_test_name(state.module_path, state.test_name);
        let prefix_visible = prefix.chars().count();
        let trailing_visible = trailing.chars().count();
        let budget = cols
            .saturating_sub(1) // 1 col of right-edge slack
            .saturating_sub(prefix_visible)
            .saturating_sub(STATUS_TAG_WIDTH)
            .saturating_sub(1) // space between tag and display
            .saturating_sub(MIN_TRAILING_PAD)
            .saturating_sub(trailing_visible);
        clip_to_cols(&raw, budget)
    };
    let lhs_naked = format!(
        "{prefix}{pad} {display}",
        pad = " ".repeat(STATUS_TAG_WIDTH),
    );
    let lhs_rendered = format!("{prefix}{tag} {display}");
    render_line(
        &lhs_naked,
        &lhs_rendered,
        &trailing,
        cols.saturating_sub(1).max(1),
    )
}

/// Lines emitted below a running test's status row — the test's
/// stdout/stderr appearing live, in source order. The drawer clears
/// + repaints these every 50ms tick alongside the status row.
///
/// Two soft caps keep the live region inside the viewport so the
/// cursor-up clear escape can actually reach what we painted:
/// - vertical: at most `terminal_height - LIVE_REGION_RESERVED_ROWS`
///   lines (showing the most recent tail when capped);
/// - horizontal: each line truncated to `terminal_width` chars so it
///   doesn't wrap onto a second viewport row.
/// Without these bounds the row count we track in `last_live_rows`
/// stops matching the actual viewport rows occupied by the paint —
/// the overflow scrolls permanently into scrollback and leaves stale
/// stripes in the user's scroll history.
#[doc(hidden)]
#[must_use]
#[inline]
pub fn running_output_lines(
    state: &TestState,
    color: ColorPolicy,
    cols: usize,
    height: usize,
) -> Vec<String> {
    if state.recent_output.is_empty() {
        return Vec::new();
    }
    let cap_rows = height.saturating_sub(LIVE_REGION_RESERVED_ROWS).max(1);
    let start = state.recent_output.len().saturating_sub(cap_rows);
    // 1 col of right-edge slack — same DECAWM defence as
    // `running_line`'s display clip.
    let line_budget = cols.saturating_sub(1).max(1);
    state.recent_output[start..]
        .iter()
        .map(|line| color.dim(&clip_to_cols(line, line_budget)))
        .collect()
}

/// Mini-histogram rows painted under a `[BENCH]` status row: a row
/// of `▁▂▃▄▅▆▇█` block-drawing chars (one per histogram bin, scaled
/// to bin height) plus an axis row showing the rendered range
/// `min … max`.
///
/// Same vertical-budget contract as [`running_output_lines`]: caller
/// passes `height_budget` (rows still available below the running
/// row + any stdout lines), this function returns at most 2 rows and
/// nothing when there isn't room. Each row is prefixed with the
/// runtime+tag indent so the histogram visually nests under the
/// test name, and clipped to `cols-1` (DECAWM rule) so a paint never
/// strands a wrap-overflow row in scrollback.
#[doc(hidden)]
#[must_use]
#[inline]
pub fn bench_histogram_lines(
    snap: &BenchProgressSnapshot,
    color: ColorPolicy,
    cols: usize,
    height_budget: usize,
) -> Vec<String> {
    if height_budget < 2 || cols < 20 || snap.done == 0 || snap.max == snap.min {
        return Vec::new();
    }
    let max_count = snap.histogram.iter().copied().max().unwrap_or(0);
    if max_count == 0 {
        return Vec::new();
    }
    let indent_width = RUNTIME_PREFIX_WIDTH
        .saturating_add(STATUS_TAG_WIDTH)
        .saturating_add(1);
    let line_budget = cols.saturating_sub(1).max(1);
    let body_budget = line_budget.saturating_sub(indent_width).max(1);
    let bars_width = body_budget.min(HISTOGRAM_BUCKETS);
    if bars_width == 0 {
        return Vec::new();
    }
    let levels: [char; 8] = ['\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
    let mut bars = String::new();
    for col in 0..bars_width {
        let bin = col
            .saturating_mul(HISTOGRAM_BUCKETS)
            .checked_div(bars_width)
            .unwrap_or(0)
            .min(HISTOGRAM_BUCKETS.saturating_sub(1));
        let count = snap.histogram[bin];
        if count == 0 {
            bars.push(' ');
        } else {
            let level = (count.saturating_mul(8) / max_count.max(1))
                .min(8)
                .saturating_sub(1) as usize;
            bars.push(levels[level.min(7)]);
        }
    }
    let bars_line = format!("{:width$}{bars}", "", width = indent_width);
    let bars_line = clip_to_cols(&bars_line, line_budget);

    let axis_text = format!("{:.0?} … {:.0?}", snap.min, snap.max);
    let axis_line = format!("{:width$}{axis_text}", "", width = indent_width);
    let axis_line = clip_to_cols(&axis_line, line_budget);

    vec![color.dim(&bars_line), color.dim(&axis_line)]
}

/// Clip `s` to `cols` visible characters; appends `…` when truncated
/// so the user can see the line was cut. `s` is expected to be raw
/// (no embedded ANSI escapes) — colour wrapping happens by the
/// caller after the clip.
fn clip_to_cols(text: &str, cols: usize) -> String {
    if cols == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= cols {
        return text.to_owned();
    }
    let take = cols.saturating_sub(1);
    let mut out: String = text.chars().take(take).collect();
    out.push('\u{2026}');
    out
}

fn lifecycle_line(
    runtime: &str,
    lifecycle: &SlotLifecycle,
    color: ColorPolicy,
    cols: usize,
) -> String {
    let prefix = format!("{runtime:<RUNTIME_PREFIX_WIDTH$}");
    let tag = render_status_tag(StatusLabel::Run, color);
    let phase_word = match lifecycle.kind {
        LifecyclePhase::Setup => "setup",
        LifecyclePhase::Teardown => "teardown",
    };
    let raw_display = format!(
        "{phase_word} {}",
        normalize_module_path(lifecycle.suite)
    );
    let elapsed = lifecycle.started_at.elapsed();
    let trailing = format!("<{elapsed:.2?}>");
    let display = {
        let prefix_visible = prefix.chars().count();
        let trailing_visible = trailing.chars().count();
        let budget = cols
            .saturating_sub(1) // DECAWM right-edge slack
            .saturating_sub(prefix_visible)
            .saturating_sub(STATUS_TAG_WIDTH)
            .saturating_sub(1)
            .saturating_sub(MIN_TRAILING_PAD)
            .saturating_sub(trailing_visible);
        clip_to_cols(&raw_display, budget)
    };
    let lhs_naked = format!(
        "{prefix}{pad} {display}",
        pad = " ".repeat(STATUS_TAG_WIDTH),
    );
    let lhs_rendered = format!("{prefix}{tag} {display}");
    render_line(
        &lhs_naked,
        &lhs_rendered,
        &trailing,
        cols.saturating_sub(1).max(1),
    )
}

/// Spawn the drawer thread. Returns the join handle; the caller
/// (the [`crate::output::CaptureGuard`]) is responsible for joining
/// it during its drop.
#[inline]
pub fn spawn_drawer(drawer: Drawer) -> IoResult<JoinHandle<()>> {
    thread::Builder::new()
        .name("rudzio-output-drawer".to_owned())
        .spawn(move || drawer.run())
}
