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
use std::io::Write as _;
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};

use super::color::ColorPolicy;
use super::events::{LifecycleEvent, PipeChunk, StdStream, TestId, TestState, TestStateKind};
use crate::config::{Format, OutputMode};
use crate::suite::{TeardownResult, TestOutcome};

const REDRAW_INTERVAL: Duration = Duration::from_millis(50);
const HINT_MAX_WIDTH: usize = 120;

/// Persistent drawer state. Constructed and handed to
/// [`spawn_drawer`]; the main loop lives in [`Drawer::run`].
#[derive(Debug)]
pub struct Drawer {
    lifecycle_rx: Receiver<LifecycleEvent>,
    pipe_rx: Receiver<PipeChunk>,
    shutdown_rx: Receiver<()>,
    terminal: std::fs::File,
    output_mode: OutputMode,
    format: Format,
    color: ColorPolicy,
    slot_order: Vec<ThreadId>,
    slots: HashMap<ThreadId, RuntimeSlot>,
    tests: HashMap<TestId, TestState>,
    thread_to_test: HashMap<ThreadId, TestId>,
    last_live_rows: usize,
    summary: Summary,
}

#[derive(Debug)]
struct RuntimeSlot {
    runtime_name: &'static str,
    current: Option<TestId>,
    /// Suite-lifecycle activity occupying this slot. While a suite's
    /// setup or teardown is in flight, no test runs on the slot's
    /// thread, so the live region renders this in place of the
    /// usual running-test row.
    lifecycle: Option<SlotLifecycle>,
}

/// A suite-level operation currently occupying a runtime slot. The
/// drawer paints this in the live region with an elapsed counter
/// just like a running test.
#[derive(Debug, Clone, Copy)]
struct SlotLifecycle {
    kind: LifecyclePhase,
    suite: &'static str,
    started_at: Instant,
}

#[derive(Debug, Clone, Copy)]
enum LifecyclePhase {
    Setup,
    Teardown,
}

#[derive(Debug, Default)]
struct Summary {
    passed: usize,
    failed: usize,
    ignored: usize,
    timed_out: usize,
    panicked: usize,
    cancelled: usize,
    /// Tests escalated past `--phase-hang-grace`. Counted separately
    /// so the summary line can show `N hung` distinct from `N timed
    /// out` and the renderer can paint a red `[HANG]` tag.
    hung: usize,
    benched: usize,
    teardown_failures: usize,
    failures: Vec<FailureRecord>,
    started_at: Option<Instant>,
}

#[derive(Debug)]
struct FailureRecord {
    display_name: String,
    outcome_label: &'static str,
    message: String,
    captured_stderr: String,
    captured_stdout: String,
}

impl Drawer {
    /// Build a drawer. Slots are allocated lazily as `TestStarted`
    /// events come in — one per distinct `ThreadId`, in first-seen
    /// order — so the runner doesn't have to know runtime names
    /// up-front.
    #[must_use]
    pub fn new(
        lifecycle_rx: Receiver<LifecycleEvent>,
        pipe_rx: Receiver<PipeChunk>,
        shutdown_rx: Receiver<()>,
        terminal: std::fs::File,
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
        }
    }

    /// Main loop: `select!` over all input channels plus a redraw
    /// timer until shutdown. On exit, drain pending events, clear
    /// the live region, and print the final summary.
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
            LifecycleEvent::BenchProgress {
                test_id,
                done,
                total,
            } => {
                if let Some(state) = self.tests.get_mut(&test_id) {
                    state.kind = TestStateKind::Bench { done, total };
                }
            }
            LifecycleEvent::TestIgnored {
                module_path,
                test_name,
                runtime_name,
                reason,
            } => {
                self.summary.ignored = self.summary.ignored.saturating_add(1);
                let display = format!("{module_path}::{test_name}");
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
                self.handle_suite_lifecycle_start(LifecyclePhase::Setup, runtime_name, suite, thread, at);
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
                let display = format!("{module_path}::{test_name}");
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
                self.summary.teardown_failures =
                    self.summary.teardown_failures.saturating_add(1);
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
                            display_name: format!("{}::{}", state.module_path, state.test_name),
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
                    if let Some(slot) = self.slots.get_mut(&state.thread) {
                        if slot.current == Some(test_id) {
                            slot.current = None;
                        }
                    }
                }
            }
        }
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
            let line = format!(
                "{phase_word:<8} {suite} ... started <{runtime_name}>\n",
            );
            let _unused = self.terminal.write_all(line.as_bytes());
        }
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
            (LifecyclePhase::Setup, Some(LifecycleFailure::TimedOut(_)))
            | (LifecyclePhase::Teardown, Some(LifecycleFailure::TimedOut(_))) => {
                StatusLabel::Timeout
            }
            (LifecyclePhase::Setup, Some(LifecycleFailure::Hung(_)))
            | (LifecyclePhase::Teardown, Some(LifecycleFailure::Hung(_))) => {
                StatusLabel::Hang
            }
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
        let display = format!("{phase_word} {suite}");
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
                display_name: format!("{phase_word} {suite}"),
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
                .min_by_key(|id| self.tests.get(id).map(|s| s.started_at))
        };
        if let Some(id) = chosen {
            if let Some(state) = self.tests.get_mut(&id) {
                match chunk.stream {
                    StdStream::Stdout => state.stdout_buffer.extend_from_slice(&chunk.bytes),
                    StdStream::Stderr => state.stderr_buffer.extend_from_slice(&chunk.bytes),
                }
                update_last_line(&mut state.last_output_line, &chunk.bytes);
                append_complete_lines(&mut state.recent_output, &chunk.bytes);
                if matches!(self.output_mode, OutputMode::Plain) {
                    let display = format!("{}::{}", state.module_path, state.test_name);
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
        }
        // Orphan bytes (no mapped test): pass through unprefixed.
        // Live mode: clear live region first; next redraw re-paints.
        if matches!(self.output_mode, OutputMode::Live) {
            self.clear_live_region();
        }
        let _unused = self.terminal.write_all(&chunk.bytes);
    }

    fn emit_plain_started(&mut self, test_id: TestId) {
        if let Some(state) = self.tests.get(&test_id) {
            let line = format!(
                "test {}::{} ... started [{}]\n",
                state.module_path, state.test_name, state.runtime_name,
            );
            let _unused = self.terminal.write_all(line.as_bytes());
        }
    }

    fn emit_completion_block(&mut self, state: &TestState, outcome: &TestOutcome) {
        if matches!(self.output_mode, OutputMode::Live) {
            self.clear_live_region();
        }
        let display = format!("{}::{}", state.module_path, state.test_name);
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

    fn redraw_live_region(&mut self) {
        if !matches!(self.output_mode, OutputMode::Live)
            || matches!(self.format, Format::Terse)
            || self.slot_order.is_empty()
        {
            return;
        }
        self.clear_live_region();
        let mut buf = String::new();
        buf.push_str(
            &self
                .color
                .dim("──────────────────────── running ────────────────────────"),
        );
        buf.push('\n');
        let mut rows = 1_usize;
        for thread in &self.slot_order {
            let slot = self.slots.get(thread);
            let (status_line, hint_lines) = if let Some(s) = slot
                && let Some(lifecycle) = s.lifecycle
            {
                (
                    lifecycle_line(s.runtime_name, &lifecycle, self.color),
                    Vec::new(),
                )
            } else if let Some((s, state)) = slot.and_then(|s| {
                s.current
                    .and_then(|id| self.tests.get(&id).map(|st| (s, st)))
            }) {
                (
                    running_line(s.runtime_name, state, self.color),
                    running_hint_lines(state, self.color),
                )
            } else {
                let name = slot.map_or("unknown", |s| s.runtime_name);
                (idle_line(name, self.color), Vec::new())
            };
            buf.push_str(&status_line);
            buf.push('\n');
            rows = rows.saturating_add(1);
            for line in &hint_lines {
                buf.push_str(line);
                buf.push('\n');
                rows = rows.saturating_add(1);
            }
        }
        let _unused = self.terminal.write_all(buf.as_bytes());
        let _unused = self.terminal.flush();
        self.last_live_rows = rows;
    }

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

    fn print_final_summary(&mut self) {
        let total_elapsed = self
            .summary
            .started_at
            .map_or(Duration::ZERO, |t| t.elapsed());
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
}

impl Summary {
    fn record_outcome(&mut self, outcome: &TestOutcome) {
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

fn is_failure(outcome: &TestOutcome) -> bool {
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

/// Fixed visible width of the status-tag column (including trailing
/// pad so the rest of the line starts at a stable column).
pub const STATUS_TAG_WIDTH: usize = 9;

/// The minimum gap between the left side and the right-aligned
/// trailing `<...>` block when the line would otherwise be shorter
/// than the terminal.
const MIN_TRAILING_PAD: usize = 2;

#[derive(Debug, Clone, Copy)]
enum StatusLabel {
    Ok,
    Fail,
    Panic,
    Timeout,
    Ignore,
    Cancel,
    Bench,
    BenchErr,
    Run,
    Idle,
    /// Failed test outcome where the per-test context (`Suite::context`)
    /// returned `Err` before the body could run. Distinct from `Fail`
    /// so the user sees that the test never executed.
    Setup,
    /// Suite-level setup completed successfully — bright variant of
    /// `Ok` reserved for lifecycle lines so they stand apart from the
    /// per-test stream.
    SetupOk,
    /// Test or teardown blew its budget AND remained pending past the
    /// Layer-2 grace window. The wrapper has fired its abort handle
    /// and moved on. Painted **red** (failure-class) so it's distinct
    /// from `Timeout`'s yellow (warn-class).
    Hang,
}

/// What went wrong in a suite-lifecycle finish event.
#[derive(Debug)]
enum LifecycleFailure {
    Error(String),
    Panicked(String),
    TimedOut(String),
    /// Phase escalated past `--phase-hang-grace`. The wrapper fired
    /// the abort handle; on tokio the spawned task drops on next
    /// poll, on other runtimes it leaks until process exit.
    Hung(String),
}

fn status_label_from_outcome(outcome: &TestOutcome) -> StatusLabel {
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
        StatusLabel::Idle => "IDLE",
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
        StatusLabel::Ignore | StatusLabel::Idle => color.dim(&naked),
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
                .map(|m| format!(", p50 {m:.2?}"))
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
    // SAFETY: ioctl TIOCGWINSZ writes a `winsize` we allocated; we
    // only read it on success. Stderr is a reliable FD to query —
    // the drawer doesn't redirect FD 2 after the capture swap
    // (FD 2 points at the write end of the stderr capture pipe, so
    // it's not useful, but FD 1 pre-capture is the terminal). We
    // walk likely-terminal FDs until one answers.
    #[allow(unsafe_code)]
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        for fd in [libc::STDERR_FILENO, libc::STDIN_FILENO, libc::STDOUT_FILENO] {
            if libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut ws) == 0 && ws.ws_col > 0 {
                return usize::from(ws.ws_col);
            }
        }
    }
    100
}

#[cfg(not(unix))]
fn terminal_width() -> usize {
    100
}

fn outcome_label(outcome: &TestOutcome) -> &'static str {
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

fn emit_captured_block(terminal: &mut std::fs::File, state: &TestState, color: ColorPolicy) {
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
    terminal: &mut std::fs::File,
    bytes: &[u8],
    _display: &str,
    stream: StdStream,
    color: ColorPolicy,
) {
    for line in split_lines(bytes) {
        let formatted = format!("  {line}\n");
        let s = match stream {
            StdStream::Stdout => formatted,
            StdStream::Stderr => color.red(&formatted),
        };
        let _unused = terminal.write_all(s.as_bytes());
    }
}

fn split_lines(bytes: &[u8]) -> impl Iterator<Item = &str> {
    std::str::from_utf8(bytes)
        .unwrap_or("")
        .split('\n')
        .filter(|l| !l.is_empty())
}

fn update_last_line(dst: &mut String, bytes: &[u8]) {
    let Ok(s) = std::str::from_utf8(bytes) else {
        return;
    };
    for line in s.split('\n') {
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

/// Append every complete, non-empty line from `bytes` to `dst`. The
/// live drawer replays all accumulated lines under each running test,
/// so this vector grows for the lifetime of a test — no size cap, no
/// per-line truncation (render-time truncation handles terminal-width
/// clipping).
fn append_complete_lines(dst: &mut Vec<String>, bytes: &[u8]) {
    let Ok(s) = std::str::from_utf8(bytes) else {
        return;
    };
    for line in s.split('\n') {
        if !line.is_empty() {
            dst.push(line.to_owned());
        }
    }
}

const RUNTIME_PREFIX_WIDTH: usize = 14;

fn idle_line(runtime: &str, color: ColorPolicy) -> String {
    let prefix = format!("{runtime:<RUNTIME_PREFIX_WIDTH$}");
    let tag = render_status_tag(StatusLabel::Idle, color);
    color.dim(&format!("{prefix}{tag}"))
}

fn running_line(runtime: &str, state: &TestState, color: ColorPolicy) -> String {
    let prefix = format!("{runtime:<RUNTIME_PREFIX_WIDTH$}");
    let label = match state.kind {
        TestStateKind::Running => StatusLabel::Run,
        TestStateKind::Bench { .. } => StatusLabel::Bench,
    };
    let tag = render_status_tag(label, color);
    let display = format!("{}::{}", state.module_path, state.test_name);
    let elapsed = state.started_at.elapsed();
    let trailing = match state.kind {
        TestStateKind::Running => format!("<{elapsed:.2?}>"),
        TestStateKind::Bench { done, total } => format!("<{done}/{total}, {elapsed:.2?}>"),
    };
    let lhs_naked = format!(
        "{prefix}{pad} {display}",
        pad = " ".repeat(STATUS_TAG_WIDTH),
    );
    let lhs_rendered = format!("{prefix}{tag} {display}");
    render_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width())
}

fn running_hint_lines(state: &TestState, color: ColorPolicy) -> Vec<String> {
    // Empty output → no hint rows. This lets the drawer collapse
    // around quiet tests instead of reserving a "(no output yet)"
    // placeholder row per running test.
    if state.recent_output.is_empty() {
        return Vec::new();
    }
    let cols = terminal_width();
    state
        .recent_output
        .iter()
        .map(|line| {
            let truncated = if line.len() > cols.saturating_sub(16) {
                &line[..cols.saturating_sub(16).min(line.len())]
            } else {
                line.as_str()
            };
            color.dim(&format!("              ↳ {truncated}"))
        })
        .collect()
}

fn lifecycle_line(runtime: &str, lifecycle: &SlotLifecycle, color: ColorPolicy) -> String {
    let prefix = format!("{runtime:<RUNTIME_PREFIX_WIDTH$}");
    let tag = render_status_tag(StatusLabel::Run, color);
    let phase_word = match lifecycle.kind {
        LifecyclePhase::Setup => "setup",
        LifecyclePhase::Teardown => "teardown",
    };
    let display = format!("{phase_word} {}", lifecycle.suite);
    let elapsed = lifecycle.started_at.elapsed();
    let trailing = format!("<{elapsed:.2?}>");
    let lhs_naked = format!(
        "{prefix}{pad} {display}",
        pad = " ".repeat(STATUS_TAG_WIDTH),
    );
    let lhs_rendered = format!("{prefix}{tag} {display}");
    render_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width())
}

/// Spawn the drawer thread. Returns the join handle; the caller
/// (the [`crate::output::CaptureGuard`]) is responsible for joining
/// it during its drop.
pub fn spawn_drawer(drawer: Drawer) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("rudzio-output-drawer".to_owned())
        .spawn(move || drawer.run())
}

/// Type alias for the lifecycle channel Sender — used by runner
/// and macro-generated code that publishes events.
pub type LifecycleSender = Sender<LifecycleEvent>;
