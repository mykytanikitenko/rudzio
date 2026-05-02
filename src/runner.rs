use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fmt::Write as _;
use std::io::{self, IsTerminal as _, Write as _};
#[cfg(unix)]
use std::mem;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::common::time::fmt_duration;
use crate::config::{CargoMeta, ColorMode, Config, Format, OutputMode, RunIgnoredMode, USAGE};
use crate::output;
use crate::output::events::LifecycleEvent;
use crate::output::{write_stderr, write_stdout};
use crate::suite::{
    Reporter as SuiteReporter, RunRequest as SuiteRunRequest, RuntimeGroupKey, RuntimeGroupOwner,
    Summary as SuiteSummary, TeardownResult, TestOutcome,
};
use crate::token::{TEST_TOKENS, Token as TestToken};

// ---------------------------------------------------------------------------
// Constants for new-format rendering
// ---------------------------------------------------------------------------
//
// Target output:
//
//   [OK]      module::path::test_name                 <runtime, 142ms>
//   [FAIL]    module::path::other_test                <runtime, 50ms>
//   [BENCH]   module::path::bench                     <runtime, 312ms, Sequential(1000), p50 1.4µs>
//
// Status labels sit in a fixed-width left column so names align; the
// trailing `<...>` block is right-aligned to the terminal width.

/// Minimum column padding between name and `<...>` info block when
/// nothing forces a wider layout.
const MIN_TRAILING_PAD: usize = 2;

/// Max visible width (brackets inclusive) of any status label we emit,
/// used to pad so everything after lines up.
const STATUS_TAG_WIDTH: usize = 9; // `[TIMEOUT]` / `[CANCEL] ` / `[IGNORE] `

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One captured failure used by the plain-mode reporter to print the
/// post-run summary block.
#[derive(Debug)]
struct FailureInfo {
    /// Human-readable failure message.
    message: String,
    /// Test display name.
    name: &'static str,
}

/// Reporter used by the runner. Behaviour depends on
/// [`OutputMode`] captured at construction:
///
/// - [`OutputMode::Plain`]: renders cargo-test-style lines directly
///   to stdout; accumulates failure details for the end-of-run
///   summary.
/// - [`OutputMode::Live`]: forwards events to the drawer via the
///   lifecycle channel (for `report_ignored` / `report_cancelled`);
///   `report_outcome` is a no-op because the macro-generated
///   dispatch already emits `TestCompleted` with the full outcome.
struct ModeReporter {
    /// Plain-mode rendering state; `None` when running in live mode.
    plain: Option<PlainState>,
    /// Count of per-test teardown failures (Err or panic). The codegen
    /// calls [`Self::report_test_teardown_failure`] from inside each
    /// test's dispatch fn, where the per-thread `SuiteSummary` is not
    /// in scope; this atomic lets the runner fold them into the
    /// final `TestSummary.teardown_failures` so the run exits non-zero
    /// when any per-test teardown failed.
    test_teardown_failures: AtomicUsize,
}

/// Mutable state used while rendering plain-mode output to stdout.
struct PlainState {
    /// Whether ANSI colour escapes should be emitted.
    colored: bool,
    /// Failures collected during the run, printed at the end.
    failures: Mutex<Vec<FailureInfo>>,
    /// Render format (terse `.` characters vs full pretty lines).
    fmt: Format,
}

impl PlainState {
    /// Append a failure record to the shared list, holding the mutex
    /// only for the duration of the push.
    fn record_failure(&self, info: FailureInfo) {
        let mut guard = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        guard.push(info);
    }
}

/// Status tag rendered before each result line in the plain-mode
/// renderer.
#[derive(Debug, Clone, Copy)]
enum StatusLabel {
    /// Successful benchmark run.
    Bench,
    /// Benchmark with failures or panics.
    BenchErr,
    /// Test cancelled before completion.
    Cancel,
    /// Standard test failure.
    Fail,
    /// Test exceeded its phase-hang-grace window.
    Hang,
    /// Test marked `#[ignore]` and skipped.
    Ignore,
    /// Test passed.
    Ok,
    /// Test panicked.
    Panic,
    /// Per-test setup returned `Err`.
    Setup,
    /// Test exceeded its timeout.
    Timeout,
}

/// Results of a test run.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct TestSummary {
    /// Tests cancelled before completion.
    pub cancelled: usize,
    /// Tests that failed via assertion or returned `Err`.
    pub failed: usize,
    /// Tests escalated from `TimedOut` to `Hung` because they ignored
    /// cooperative cancellation past `--phase-hang-grace`. Counted
    /// alongside `failed`/`panicked`/`timed_out` for `is_success`.
    pub hung: usize,
    /// Tests skipped via `#[ignore]`.
    pub ignored: usize,
    /// Tests that panicked.
    pub panicked: usize,
    /// Tests that passed.
    pub passed: usize,
    /// Combined count of suite-level + per-test teardown failures
    /// (Err *or* panic *or* Hung). Drives [`is_success`] so a botched
    /// cleanup fails the run even when every test body passed.
    pub teardown_failures: usize,
    /// Tests that exceeded their timeout but did not escalate to hang.
    pub timed_out: usize,
    /// Total number of tests considered (including ignored).
    pub total: usize,
}

// ---------------------------------------------------------------------------
// Impls
// ---------------------------------------------------------------------------

impl ModeReporter {
    /// Construct a reporter wired for either plain or live output mode
    /// based on `config.output_mode`.
    fn new(config: &Config) -> Self {
        match config.output_mode {
            OutputMode::Plain => Self {
                plain: Some(PlainState {
                    failures: Mutex::new(Vec::new()),
                    fmt: config.format,
                    colored: use_color(config.color),
                }),
                test_teardown_failures: AtomicUsize::new(0),
            },
            OutputMode::Live => Self {
                plain: None,
                test_teardown_failures: AtomicUsize::new(0),
            },
        }
    }
}

impl StatusLabel {
    /// Map a [`TestOutcome`] to the matching status label.
    const fn from_outcome(outcome: &TestOutcome) -> Self {
        match outcome {
            TestOutcome::Passed { .. } => Self::Ok,
            TestOutcome::Failed { .. } => Self::Fail,
            TestOutcome::Panicked { .. } => Self::Panic,
            TestOutcome::SetupFailed { .. } => Self::Setup,
            TestOutcome::TimedOut => Self::Timeout,
            TestOutcome::Hung { .. } => Self::Hang,
            TestOutcome::Cancelled => Self::Cancel,
            TestOutcome::Benched { report, .. } => {
                if report.failures.is_empty() && report.panics == 0 {
                    Self::Bench
                } else {
                    Self::BenchErr
                }
            }
        }
    }
}

impl SuiteReporter for ModeReporter {
    fn report_cancelled(&self, token: &'static TestToken, runtime_name: &'static str) {
        if let Some(plain) = &self.plain {
            match plain.fmt {
                Format::Terse => {
                    write_stdout(&yellow("c", plain.colored));
                    let _flush = io::stdout().flush();
                }
                Format::Pretty => {
                    let (tag_rendered, tag_visible) =
                        status_tag(StatusLabel::Cancel, plain.colored);
                    let display = qualified_test_name(token.module_path, token.name);
                    let lhs_naked = format!("{:width$} {display}", "", width = tag_visible);
                    let lhs_rendered = format!("{tag_rendered} {display}");
                    let trailing = runtime_only_info(runtime_name);
                    let line =
                        render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                    write_stdout(&format!("{line}\n"));
                }
            }
            return;
        }
        output::send_lifecycle(LifecycleEvent::TestIgnored {
            module_path: token.module_path,
            test_name: token.name,
            runtime_name,
            reason: "cancelled before dispatch",
        });
    }

    fn report_ignored(&self, token: &'static TestToken, runtime_name: &'static str) {
        if let Some(plain) = &self.plain {
            match plain.fmt {
                Format::Terse => {
                    write_stdout(&yellow("i", plain.colored));
                    let _flush = io::stdout().flush();
                }
                Format::Pretty => {
                    let (tag_rendered, tag_visible) =
                        status_tag(StatusLabel::Ignore, plain.colored);
                    let display = qualified_test_name(token.module_path, token.name);
                    let trailing = if token.ignore_reason.is_empty() {
                        runtime_only_info(runtime_name)
                    } else {
                        format!("<{runtime_name}, {}>", token.ignore_reason)
                    };
                    let lhs_naked = format!("{:width$} {display}", "", width = tag_visible);
                    let lhs_rendered = format!("{tag_rendered} {display}");
                    let line =
                        render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                    write_stdout(&format!("{line}\n"));
                }
            }
            return;
        }
        output::send_lifecycle(LifecycleEvent::TestIgnored {
            module_path: token.module_path,
            test_name: token.name,
            runtime_name,
            reason: token.ignore_reason,
        });
    }

    fn report_outcome(
        &self,
        token: &'static TestToken,
        runtime_name: &'static str,
        outcome: TestOutcome,
    ) {
        let Some(plain) = &self.plain else {
            // Live mode: macro-generated dispatch already emitted
            // TestCompleted with the full outcome; nothing to do.
            return;
        };
        match plain.fmt {
            Format::Terse => {
                let glyph = match &outcome {
                    TestOutcome::Passed { .. } => ".".to_owned(),
                    TestOutcome::Failed { .. }
                    | TestOutcome::Panicked { .. }
                    | TestOutcome::SetupFailed { .. }
                    | TestOutcome::TimedOut
                    | TestOutcome::Hung { .. } => red("F", plain.colored),
                    TestOutcome::Cancelled => yellow("c", plain.colored),
                    TestOutcome::Benched { report, .. } => {
                        if report.is_success() {
                            "b".to_owned()
                        } else {
                            red("B", plain.colored)
                        }
                    }
                };
                write_stdout(&glyph);
                let _flush = io::stdout().flush();
            }
            Format::Pretty => {
                let block = pretty_outcome_block(token, runtime_name, &outcome, plain.colored);
                write_stdout(&format!("{block}\n"));
            }
        }

        match outcome {
            TestOutcome::Failed { message, .. } => {
                plain.record_failure(FailureInfo {
                    name: token.name,
                    message,
                });
            }
            TestOutcome::SetupFailed { message, .. } => {
                plain.record_failure(FailureInfo {
                    name: token.name,
                    message: format!("test setup failed: {message}"),
                });
            }
            TestOutcome::Benched { report, .. } if !report.is_success() => {
                let message = format!(
                    "benchmark {} reported {} failed iterations and {} panics:\n{}",
                    report.strategy,
                    report.failures.len(),
                    report.panics,
                    report.failures.join("\n"),
                );
                plain.record_failure(FailureInfo {
                    name: token.name,
                    message,
                });
            }
            TestOutcome::Benched { .. }
            | TestOutcome::Cancelled
            | TestOutcome::Hung { .. }
            | TestOutcome::Panicked { .. }
            | TestOutcome::Passed { .. }
            | TestOutcome::TimedOut => {}
        }
    }

    fn report_suite_setup_finished(
        &self,
        runtime_name: &'static str,
        suite: &'static str,
        elapsed: Duration,
        error: Option<&str>,
    ) {
        if let Some(plain) = &self.plain {
            if matches!(plain.fmt, Format::Pretty) {
                // Suite-level setup failure: render as [FAIL]. The
                // [SETUP] tag is reserved for per-test
                // SetupFailed outcomes, where it visually
                // distinguishes a context-creation miss from a
                // body-level [FAIL].
                let label = if error.is_some() {
                    StatusLabel::Fail
                } else {
                    StatusLabel::Ok
                };
                let (tag_rendered, tag_visible) = status_tag(label, plain.colored);
                let display = format!("setup {}", normalize_module_path(suite));
                let trailing = format!("<{runtime_name}, {}>", format_elapsed(elapsed));
                let lhs_naked = format!("{:width$} {display}", "", width = tag_visible);
                let lhs_rendered = format!("{tag_rendered} {display}");
                let line =
                    render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                write_stdout(&format!("{line}\n"));
                if let Some(msg) = error {
                    write_stdout(&format!(
                        "  {}\n",
                        red(&format!("error: {msg}"), plain.colored)
                    ));
                }
            }
            if let Some(msg) = error {
                plain.record_failure(FailureInfo {
                    name: "<suite setup>",
                    message: format!(
                        "setup {} [{runtime_name}]: {msg}",
                        normalize_module_path(suite)
                    ),
                });
            }
            return;
        }
        output::send_lifecycle(LifecycleEvent::SuiteSetupFinished {
            runtime_name,
            suite,
            thread: thread::current().id(),
            elapsed,
            error: error.map(str::to_owned),
        });
    }

    fn report_suite_setup_started(&self, runtime_name: &'static str, suite: &'static str) {
        if let Some(plain) = &self.plain {
            if matches!(plain.fmt, Format::Pretty) {
                let suite_disp = normalize_module_path(suite);
                write_stdout(&format!(
                    "setup    {suite_disp} ... started <{runtime_name}>\n"
                ));
            }
            return;
        }
        output::send_lifecycle(LifecycleEvent::SuiteSetupStarted {
            runtime_name,
            suite,
            thread: thread::current().id(),
            at: Instant::now(),
        });
    }

    fn report_suite_teardown_finished(
        &self,
        runtime_name: &'static str,
        suite: &'static str,
        elapsed: Duration,
        result: TeardownResult,
    ) {
        if let Some(plain) = &self.plain {
            let suite_disp = normalize_module_path(suite);
            if matches!(plain.fmt, Format::Pretty) {
                let label = match result {
                    TeardownResult::Ok => StatusLabel::Ok,
                    TeardownResult::Err(_) => StatusLabel::Fail,
                    TeardownResult::Panicked(_) => StatusLabel::Panic,
                    TeardownResult::TimedOut => StatusLabel::Timeout,
                    TeardownResult::Hung => StatusLabel::Hang,
                };
                let (tag_rendered, tag_visible) = status_tag(label, plain.colored);
                let display = format!("teardown {suite_disp}");
                let trailing = format!("<{runtime_name}, {}>", format_elapsed(elapsed));
                let lhs_naked = format!("{:width$} {display}", "", width = tag_visible);
                let lhs_rendered = format!("{tag_rendered} {display}");
                let line =
                    render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                write_stdout(&format!("{line}\n"));
                match &result {
                    TeardownResult::Ok => {}
                    TeardownResult::Err(msg) => {
                        write_stdout(&format!(
                            "  {}\n",
                            red(&format!("error: {msg}"), plain.colored)
                        ));
                    }
                    TeardownResult::Panicked(msg) => {
                        write_stdout(&format!(
                            "  {}\n",
                            red(&format!("panic: {msg}"), plain.colored)
                        ));
                    }
                    TeardownResult::TimedOut => {
                        write_stdout(&format!(
                            "  {}\n",
                            red("timeout: teardown timed out", plain.colored)
                        ));
                    }
                    TeardownResult::Hung => {
                        write_stdout(&format!(
                            "  {}\n",
                            red("hang: teardown hung; abort signal sent", plain.colored)
                        ));
                    }
                }
            }
            match result {
                TeardownResult::Ok => {}
                TeardownResult::Err(msg) => {
                    plain.record_failure(FailureInfo {
                        name: "<suite teardown>",
                        message: format!("teardown {suite_disp} [{runtime_name}]: {msg}"),
                    });
                }
                TeardownResult::Panicked(msg) => {
                    plain.record_failure(FailureInfo {
                        name: "<suite teardown>",
                        message: format!("teardown {suite_disp} [{runtime_name}]: panic: {msg}"),
                    });
                }
                TeardownResult::TimedOut => {
                    plain.record_failure(FailureInfo {
                        name: "<suite teardown>",
                        message: format!(
                            "teardown {suite_disp} [{runtime_name}]: timeout: teardown timed out"
                        ),
                    });
                }
                TeardownResult::Hung => {
                    plain.record_failure(FailureInfo {
                        name: "<suite teardown>",
                        message: format!(
                            "teardown {suite_disp} [{runtime_name}]: hang: teardown hung; abort signal sent"
                        ),
                    });
                }
            }
            return;
        }
        output::send_lifecycle(LifecycleEvent::SuiteTeardownFinished {
            runtime_name,
            suite,
            thread: thread::current().id(),
            elapsed,
            result,
        });
    }

    fn report_suite_teardown_started(&self, runtime_name: &'static str, suite: &'static str) {
        if let Some(plain) = &self.plain {
            if matches!(plain.fmt, Format::Pretty) {
                let suite_disp = normalize_module_path(suite);
                write_stdout(&format!(
                    "teardown {suite_disp} ... started <{runtime_name}>\n"
                ));
            }
            return;
        }
        output::send_lifecycle(LifecycleEvent::SuiteTeardownStarted {
            runtime_name,
            suite,
            thread: thread::current().id(),
            at: Instant::now(),
        });
    }

    fn report_test_teardown_failure(
        &self,
        token: &'static TestToken,
        runtime_name: &'static str,
        result: TeardownResult,
    ) {
        if !matches!(result, TeardownResult::Ok) {
            let _prev = self.test_teardown_failures.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(plain) = &self.plain {
            let display = qualified_test_name(token.module_path, token.name);
            if matches!(plain.fmt, Format::Pretty) {
                let label = match result {
                    TeardownResult::Ok => return,
                    TeardownResult::Err(_) => StatusLabel::Fail,
                    TeardownResult::Panicked(_) => StatusLabel::Panic,
                    TeardownResult::TimedOut => StatusLabel::Timeout,
                    TeardownResult::Hung => StatusLabel::Hang,
                };
                let (tag_rendered, tag_visible) = status_tag(label, plain.colored);
                let lhs_display = format!("teardown {display}");
                let trailing = format!("<{runtime_name}>");
                let lhs_naked = format!("{:width$} {lhs_display}", "", width = tag_visible);
                let lhs_rendered = format!("{tag_rendered} {lhs_display}");
                let line =
                    render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                write_stdout(&format!("{line}\n"));
                match &result {
                    TeardownResult::Ok => {}
                    TeardownResult::Err(msg) => {
                        write_stdout(&format!(
                            "  {}\n",
                            red(&format!("error: {msg}"), plain.colored)
                        ));
                    }
                    TeardownResult::Panicked(msg) => {
                        write_stdout(&format!(
                            "  {}\n",
                            red(&format!("panic: {msg}"), plain.colored)
                        ));
                    }
                    TeardownResult::TimedOut => {
                        write_stdout(&format!(
                            "  {}\n",
                            red("timeout: teardown timed out", plain.colored)
                        ));
                    }
                    TeardownResult::Hung => {
                        write_stdout(&format!(
                            "  {}\n",
                            red("hang: teardown hung; abort signal sent", plain.colored)
                        ));
                    }
                }
            }
            match result {
                TeardownResult::Ok => {}
                TeardownResult::Err(msg) => {
                    plain.record_failure(FailureInfo {
                        name: token.name,
                        message: format!("test teardown failed [{runtime_name}]: {msg}"),
                    });
                }
                TeardownResult::Panicked(msg) => {
                    plain.record_failure(FailureInfo {
                        name: token.name,
                        message: format!("test teardown panicked [{runtime_name}]: {msg}"),
                    });
                }
                TeardownResult::TimedOut => {
                    plain.record_failure(FailureInfo {
                        name: token.name,
                        message: format!(
                            "test teardown timed out [{runtime_name}]: teardown timed out"
                        ),
                    });
                }
                TeardownResult::Hung => {
                    plain.record_failure(FailureInfo {
                        name: token.name,
                        message: format!(
                            "test teardown hung [{runtime_name}]: hang: teardown hung; abort signal sent"
                        ),
                    });
                }
            }
            return;
        }
        output::send_lifecycle(LifecycleEvent::TestTeardownFailed {
            module_path: token.module_path,
            test_name: token.name,
            runtime_name,
            result,
        });
    }

    fn report_warning(&self, message: &str) {
        write_stderr(&format!("warning: {message}\n"));
    }
}

impl TestSummary {
    #[inline]
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        if self.is_success() { 0 } else { 1 }
    }

    #[inline]
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.failed == 0
            && self.timed_out == 0
            && self.hung == 0
            && self.panicked == 0
            && self.cancelled == 0
            && self.teardown_failures == 0
    }

    #[inline]
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        Self {
            cancelled: self.cancelled.saturating_add(other.cancelled),
            failed: self.failed.saturating_add(other.failed),
            hung: self.hung.saturating_add(other.hung),
            ignored: self.ignored.saturating_add(other.ignored),
            panicked: self.panicked.saturating_add(other.panicked),
            passed: self.passed.saturating_add(other.passed),
            timed_out: self.timed_out.saturating_add(other.timed_out),
            total: self.total.saturating_add(other.total),
            teardown_failures: self
                .teardown_failures
                .saturating_add(other.teardown_failures),
        }
    }

    #[inline]
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            cancelled: 0,
            failed: 0,
            hung: 0,
            ignored: 0,
            panicked: 0,
            passed: 0,
            timed_out: 0,
            total: 0,
            teardown_failures: 0,
        }
    }
}

impl From<SuiteSummary> for TestSummary {
    #[inline]
    fn from(summary: SuiteSummary) -> Self {
        Self {
            cancelled: summary.cancelled,
            failed: summary.failed,
            hung: summary.hung,
            ignored: summary.ignored,
            panicked: summary.panicked,
            passed: summary.passed,
            timed_out: summary.timed_out,
            total: summary.total,
            teardown_failures: summary.teardown_failures,
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions (alphabetical)
// ---------------------------------------------------------------------------

/// Wrap `text` with the bold ANSI SGR code when `colored` is true.
fn bold(text: &str, colored: bool) -> String {
    paint(text, "1", colored)
}

/// Set `RUST_BACKTRACE=full` and `RUST_LIB_BACKTRACE=full` if neither
/// is already in the env, so panic messages always carry an actionable
/// backtrace under the rudzio runner. When the binary is launched
/// directly (no rudzio runner), this fn is never called and stdlib's
/// normal env-var lookup applies.
fn enable_full_backtrace_default() {
    if env::var_os("RUST_BACKTRACE").is_none() {
        #[expect(
            unsafe_code,
            reason = "single-threaded entry point; see SAFETY comment below"
        )]
        // SAFETY: rudzio's entry point runs before any test threads spawn,
        // so no other thread is mutating the environment concurrently.
        // `env::set_var` is sound under that single-threaded invariant.
        unsafe {
            env::set_var("RUST_BACKTRACE", "full");
        }
    }
    if env::var_os("RUST_LIB_BACKTRACE").is_none() {
        #[expect(
            unsafe_code,
            reason = "single-threaded entry point; see SAFETY comment below"
        )]
        // SAFETY: rudzio's entry point runs before any test threads spawn,
        // so no other thread is mutating the environment concurrently.
        // `env::set_var` is sound under that single-threaded invariant.
        unsafe {
            env::set_var("RUST_LIB_BACKTRACE", "full");
        }
    }
}

/// Format `elapsed` for the trailing `<runtime, …>` block using a
/// short `1.23s`-style representation.
fn format_elapsed(elapsed: Duration) -> String {
    fmt_duration(elapsed)
}

/// Wrap `text` with the green ANSI SGR code when `colored` is true.
fn green(text: &str, colored: bool) -> String {
    paint(text, "32", colored)
}

/// Install SIGINT/SIGTERM handlers that flip `token`, giving in-flight
/// tests a chance to observe cooperative cancellation.
#[cfg(unix)]
fn install_signal_handler(token: CancellationToken) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(handle) => handle,
        Err(err) => {
            write_stderr(&format!(
                "warning: failed to install signal handler: {err}\n"
            ));
            return;
        }
    };
    let _handle = thread::Builder::new()
        .name("rudzio-signal-handler".to_owned())
        .spawn(move || {
            if let Some(signal) = signals.forever().next() {
                let name = match signal {
                    SIGINT => "SIGINT",
                    SIGTERM => "SIGTERM",
                    _ => "unknown signal",
                };
                write_stderr(&format!("\nreceived {name}, cancelling run...\n"));
                token.cancel();
            }
        });
}

#[cfg(not(unix))]
fn install_signal_handler(_token: CancellationToken) {}

/// Strip rudzio-autogenerated segments from a `module_path!()` string
/// so the displayed test path begins at the user's crate or module
/// name. Drops:
///
/// 1. The leading segment — always the crate where the test was
///    compiled. In per-crate mode this is `main` (the cargo
///    `[[test]] name = "main"` test binary). In the workspace
///    aggregator it is the aggregator crate name. In neither mode
///    does the segment carry information the user wrote or expects.
/// 2. The literal `tests` segment when it appears immediately after
///    the dropped crate segment — only the aggregator emits a
///    `mod tests` wrapper around its member crates.
/// 3. Any further segment named `main`. The aggregator mounts each
///    member's `tests/main.rs` shim as `mod main`, and the same name
///    is the cargo test-binary convention; both are autogenerated
///    rather than user-authored.
///
/// Empty input or fully-stripped paths return `""`.
#[doc(hidden)]
#[must_use]
#[inline]
pub fn normalize_module_path(mp: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut just_dropped_crate = false;
    for (i, seg) in mp.split("::").enumerate() {
        if i == 0 {
            just_dropped_crate = true;
            continue;
        }
        if just_dropped_crate && seg == "tests" {
            just_dropped_crate = false;
            continue;
        }
        just_dropped_crate = false;
        if seg == "main" {
            continue;
        }
        out.push(seg);
    }
    out.join("::")
}

/// Build the multi-line Pretty-mode block for a finished test
/// (status line + optional bench stats + histogram or inlined failure
/// message). The trailing newline is added by the caller.
fn pretty_outcome_block(
    token: &'static TestToken,
    runtime_name: &'static str,
    outcome: &TestOutcome,
    colored: bool,
) -> String {
    let label = StatusLabel::from_outcome(outcome);
    let (tag_rendered, tag_visible) = status_tag(label, colored);
    let display = qualified_test_name(token.module_path, token.name);
    let trailing = trailing_info(outcome, runtime_name);
    let lhs_naked = format!("{:width$} {display}", "", width = tag_visible);
    let lhs_rendered = format!("{tag_rendered} {display}");
    let header = render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
    let mut buf = header;
    if let TestOutcome::Benched { report, .. } = outcome {
        // Bench status line + detailed stats + histogram, emitted as
        // a single atomic write so concurrent runtime threads can't
        // interleave each other's blocks.
        buf.push('\n');
        buf.push_str(report.detailed_summary().trim_end_matches('\n'));
        if report.failures.is_empty() && report.panics == 0 {
            let histogram = report.ascii_histogram(10, 30);
            if !histogram.is_empty() {
                buf.push('\n');
                buf.push_str("  histogram:\n");
                buf.push_str(histogram.trim_end_matches('\n'));
            }
        }
    } else if let Some(msg) = outcome_inline_message(outcome) {
        // Single atomic write: header + inlined failure message,
        // rendered in the tag's color for failing outcomes so the
        // reason is visible alongside the status line.
        for line in msg.lines() {
            buf.push('\n');
            let body = format!("  {line}");
            let painted = if matches!(
                label,
                StatusLabel::Fail
                    | StatusLabel::Panic
                    | StatusLabel::Setup
                    | StatusLabel::Timeout
                    | StatusLabel::Cancel
            ) {
                red(&body, colored)
            } else {
                body
            };
            buf.push_str(&painted);
        }
    } else {
        // Passed / Ignored / etc.; the status line is the whole payload.
    }
    buf
}

/// One-shot diagnostic message for a finished test — rendered
/// indented right under its status line so the reason for failure
/// (test body error, setup error, panic payload, timeout note)
/// is visible without scrolling to the end-of-run failures section.
fn outcome_inline_message(outcome: &TestOutcome) -> Option<String> {
    match outcome {
        TestOutcome::Failed { message, .. } => Some(message.clone()),
        TestOutcome::SetupFailed { message, .. } => Some(format!("test setup failed: {message}")),
        TestOutcome::TimedOut => Some("test exceeded its timeout".to_owned()),
        TestOutcome::Hung { .. } => Some("hung; abort signal sent".to_owned()),
        TestOutcome::Cancelled => Some("test was cancelled before completion".to_owned()),
        TestOutcome::Panicked { .. } | TestOutcome::Passed { .. } | TestOutcome::Benched { .. } => {
            None
        }
    }
}

/// Wrap `text` with the given ANSI SGR `code` when `colored` is true,
/// otherwise return `text` unchanged.
fn paint(text: &str, code: &str, colored: bool) -> String {
    if colored {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Format a token's display name as the runner shows it everywhere:
/// the normalized module path joined to the test name with `::`.
/// When normalization strips the path to nothing, returns just the
/// test name (no leading separator).
#[doc(hidden)]
#[must_use]
#[inline]
pub fn qualified_test_name(module_path: &str, test_name: &str) -> String {
    let normalized = normalize_module_path(module_path);
    if normalized.is_empty() {
        test_name.to_owned()
    } else {
        format!("{normalized}::{test_name}")
    }
}

/// Wrap `text` with the red ANSI SGR code when `colored` is true.
fn red(text: &str, colored: bool) -> String {
    paint(text, "31", colored)
}

/// Right-align `trailing` to the terminal width, with at least
/// [`MIN_TRAILING_PAD`] spaces between `lhs` and `trailing`. `lhs`
/// already includes the status tag + space + display name; its
/// visible width is passed separately so ANSI escapes don't skew the
/// column math.
fn render_status_line(
    lhs_naked: &str,
    lhs_rendered: &str,
    trailing: &str,
    term_cols: usize,
) -> String {
    let lhs_visible = lhs_naked.chars().count();
    let trailing_visible = trailing.chars().count();
    let pad = term_cols
        .saturating_sub(lhs_visible)
        .saturating_sub(trailing_visible)
        .max(MIN_TRAILING_PAD);
    let mut out = String::with_capacity(
        lhs_rendered
            .len()
            .saturating_add(pad)
            .saturating_add(trailing.len()),
    );
    out.push_str(lhs_rendered);
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(trailing);
    out
}

/// Collect all registered [`TestToken`]s and dispatch them.
///
/// Groups them by `runtime_group_key`, runs each group in its own OS
/// thread via its [`RuntimeGroupOwner`](crate::suite::RuntimeGroupOwner),
/// and renders per-test output according to [`Config::output_mode`]:
///
/// - [`OutputMode::Live`]: the bottom-of-terminal live region + append
///   history (see `crate::output::render`).
/// - [`OutputMode::Plain`]: classic cargo-test-style lines; the
///   runner prints them directly from its reporter.
///
/// `cargo` comes from the caller (the `#[rudzio::main]` macro expands
/// `cargo_meta!()` at the user's crate site so the `env!(...)` values
/// belong to that crate, not rudzio).
#[inline]
/// Plain-mode end-of-run summary: failures section followed by the
/// `test result: …` line. Live mode emits its own summary from the
/// drawer instead.
fn print_plain_summary(
    plain: &PlainState,
    grand_total: TestSummary,
    total_count: usize,
    filtered_out: usize,
    elapsed: Duration,
    colored_plain: bool,
) {
    if plain.fmt == Format::Terse && total_count > 0 {
        write_stdout("\n");
    }
    let guard = plain
        .failures
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !guard.is_empty() {
        write_stdout("\nfailures:\n\n");
        for failure in guard.iter() {
            write_stdout(&format!("---- {} ----\n", failure.name));
            write_stdout(&format!("{}\n\n", failure.message));
        }
        write_stdout("failures:\n");
        for failure in guard.iter() {
            write_stdout(&format!("    {}\n", failure.name));
        }
        write_stdout("\n");
    }
    drop(guard);

    let result_label = if grand_total.is_success() {
        bold(&green("ok", colored_plain), colored_plain)
    } else {
        bold(&red("FAILED", colored_plain), colored_plain)
    };
    let elapsed_text = fmt_duration(elapsed);
    write_stdout(&format!(
        "test result: {}. {} passed; {} failed; {} panicked; {} timed out; \
         {} cancelled; {} ignored; {} teardown failed; 0 measured; {} total; \
         {} filtered out; finished in {elapsed_text}\n",
        result_label,
        grand_total.passed,
        grand_total.failed,
        grand_total.panicked,
        grand_total.timed_out,
        grand_total.cancelled,
        grand_total.ignored,
        grand_total.teardown_failures,
        grand_total.total,
        filtered_out,
    ));
}

/// Group `tokens` by `(runtime_group_key)` and dispatch one OS thread
/// per group via `thread::scope`, joining them all at scope exit. Each
/// thread borrows `&config`/`&reporter` directly instead of cloning an
/// `Arc` — the rule against `'static` substitution where stack
/// borrows suffice.
fn dispatch_test_groups(
    tokens: &[&'static TestToken],
    config: &Config,
    root_token: &CancellationToken,
    reporter: &ModeReporter,
) -> TestSummary {
    let mut groups: HashMap<RuntimeGroupKey, Vec<&'static TestToken>> = HashMap::new();
    for token in tokens {
        groups
            .entry(token.runtime_group_key)
            .or_default()
            .push(token);
    }
    thread::scope(|scope| {
        // Spawn every group's thread first; a lazy iterator + fold
        // would serialize spawn-join-spawn-join.
        let mut handles = Vec::new();
        for mut group_tokens in groups.into_values() {
            group_tokens.sort_by_key(|token| (token.file, token.line));
            let Some(first) = group_tokens.first() else {
                continue;
            };
            let owner: &'static dyn RuntimeGroupOwner = first.runtime_group_owner;
            let req_root = root_token.child_token();
            handles.push(scope.spawn(move || {
                let req = SuiteRunRequest {
                    tokens: &group_tokens,
                    config,
                    root_token: req_root,
                };
                owner.run_group(req, reporter)
            }));
        }
        let mut total = TestSummary::zero();
        for handle in handles {
            match handle.join() {
                Ok(suite_summary) => {
                    total = total.merge(TestSummary::from(suite_summary));
                }
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("unknown panic");
                    write_stderr(&format!("error: runtime thread panicked: {msg}\n"));
                    total = total.merge(TestSummary {
                        panicked: 1,
                        total: 1,
                        ..TestSummary::zero()
                    });
                }
            }
        }
        total
    })
}

/// Run-timeout watchdog: cancels `token` once `dur` elapses unless
/// the token has already been cancelled (SIGINT/SIGTERM, etc.).
fn spawn_run_timeout_watchdog(token: CancellationToken, dur: Duration) {
    let _watchdog = thread::spawn(move || {
        thread::sleep(dur);
        if !token.is_cancelled() {
            let dur_text = fmt_duration(dur);
            write_stderr(&format!(
                "\nrun timeout ({dur_text}) exceeded, cancelling run...\n"
            ));
            token.cancel();
        }
    });
}

/// Layer-1 process-exit watchdog. Listens for `token` cancellation
/// (SIGINT / SIGTERM / --run-timeout / explicit user cancel) and,
/// after `grace`, force-exits the process with code 2. The universal
/// safety net for sync-blocked tasks that ignore every cooperative
/// cancellation signal — no amount of token-listening or `abort()`
/// will free the worker, but `_exit` lets the OS reap every thread.
fn spawn_grace_force_exit_watchdog(token: CancellationToken, grace: Duration) {
    let _watchdog = thread::Builder::new()
        .name("rudzio-cancel-grace-watchdog".to_owned())
        .spawn(move || {
            // Sync poll-loop until the token is cancelled. Avoids an
            // executor dep — the watchdog runs no rudzio test code
            // itself, just times out and force-exits. 50ms tick is
            // fine: this is fault-tolerance plumbing, not a hot path.
            while !token.is_cancelled() {
                thread::sleep(Duration::from_millis(50));
            }
            thread::sleep(grace);
            let grace_text = fmt_duration(grace);
            write_stderr(&format!(
                "\nrudzio: {grace_text} grace period exceeded after cancellation, \
                 force-exiting (some phase ignored cooperative cancel)\n"
            ));
            #[expect(
                unsafe_code,
                reason = "watchdog runs on a spawned thread; \
                main may be sync-blocked, so cooperative ExitCode return cannot \
                reach the process exit. _exit avoids the clippy::exit lint while \
                preserving the deliberate force-exit semantics."
            )]
            // SAFETY: libc::_exit immediately terminates the process
            // without running destructors. It has no preconditions and
            // never returns; force-exit semantics are intentional.
            unsafe {
                libc::_exit(2);
            }
        });
}

#[must_use]
#[inline]
pub fn run(cargo: CargoMeta) -> ExitCode {
    // Default `RUST_BACKTRACE=full` (and `RUST_LIB_BACKTRACE`) **only
    // when the user hasn't set them**. Backtraces are essential for
    // diagnosing panics in async test bodies — the libtest harness
    // defaults to "short" but our drawer + capture pipeline often
    // rewrites the panic line such that the short-backtrace heuristic
    // (look-for-test-fn-name) misses, leaving the user with two lines
    // of "note: run with `RUST_BACKTRACE=...`". Set both vars so the
    // value applies to direct panics and to library code (e.g. tokio's
    // worker-thread panics) alike. Skip the override entirely when the
    // user has expressed any preference, including `RUST_BACKTRACE=0`.
    enable_full_backtrace_default();

    let config = Config::parse(cargo);

    // --help / -h: print USAGE to real stdout and exit before the
    // output-capture pipe is installed, so the help text reaches the
    // user's terminal directly.
    if config.help {
        write_stdout(USAGE);
        return ExitCode::SUCCESS;
    }

    let colored_plain = matches!(config.output_mode, OutputMode::Plain) && use_color(config.color);

    // Output capture + render. In Plain mode returns a no-op guard
    // and the reporter below prints directly. In Live mode returns
    // the real drawer-driving guard.
    let capture_guard = match output::init(&config) {
        Ok(guard) => guard,
        Err(err) => {
            write_stderr(&format!(
                "rudzio: failed to initialise output capture: {err}\n"
            ));
            return ExitCode::from(2);
        }
    };

    let all_tokens: Vec<&'static TestToken> = TEST_TOKENS.iter().collect();

    let filtered_tokens: Vec<&'static TestToken> = all_tokens
        .iter()
        .copied()
        .filter(|token| {
            let qualified = qualified_test_name(token.module_path, token.name);
            token_passes_filters(
                &qualified,
                token.ignored,
                config.filter.as_deref(),
                &config.skip_filters,
                config.run_ignored,
            )
        })
        .collect();

    let filtered_out = all_tokens.len().saturating_sub(filtered_tokens.len());

    if config.list {
        drop(capture_guard);
        for token in &filtered_tokens {
            write_stdout(&format!(
                "{}: test\n",
                qualified_test_name(token.module_path, token.name)
            ));
        }
        return ExitCode::SUCCESS;
    }

    let total_count = filtered_tokens.len();
    // Only print the "running N tests" header in Plain mode — the
    // Live drawer has its own banner inside the live region.
    if matches!(config.output_mode, OutputMode::Plain) {
        write_stdout(&format!(
            "running {} {}\n",
            total_count,
            if total_count == 1 { "test" } else { "tests" }
        ));
    }

    // Root cancellation token: cancelled on run-timeout, SIGINT, or SIGTERM.
    let root_token = CancellationToken::new();
    install_signal_handler(root_token.clone());

    if let Some(dur) = config.run_timeout {
        spawn_run_timeout_watchdog(root_token.clone(), dur);
    }
    if let Some(grace) = config.cancel_grace_period {
        spawn_grace_force_exit_watchdog(root_token.clone(), grace);
    }

    let reporter = ModeReporter::new(&config);
    let start = Instant::now();

    let total = dispatch_test_groups(&filtered_tokens, &config, &root_token, &reporter);

    // Per-test teardown failures aren't visible to the per-thread
    // SuiteSummary (the per-test fn doesn't have it in scope), so fold
    // the reporter's atomic counter in here.
    let grand_total = total.merge(TestSummary {
        teardown_failures: reporter.test_teardown_failures.load(Ordering::Relaxed),
        ..TestSummary::zero()
    });

    let elapsed = start.elapsed();

    // Plain-mode summary rendering. Live mode lets the drawer handle
    // it during its shutdown path.
    if let Some(plain) = &reporter.plain {
        print_plain_summary(
            plain,
            grand_total,
            total_count,
            filtered_out,
            elapsed,
            colored_plain,
        );
    }

    // Drop the guard — in Live mode this signals and joins the drawer
    // (which prints its own summary + restores FDs). In Plain mode
    // the guard is a no-op.
    drop(capture_guard);

    // Background-thread panic safety net: if the panic hook caught a
    // panic that no test outcome accounts for (e.g. user setup spawned
    // a thread that panicked, then the future returned Ok), surface
    // it. Otherwise a hung crypto-provider init or similar would
    // produce a "test result: ok" that silently lies. Done after the
    // capture guard drop so the warning lands on the real terminal,
    // never inside the live region.
    let bg_panics = output::panic_hook::unattributed_panic_count();
    if bg_panics > 0 && grand_total.is_success() {
        write_stderr(&format!(
            "rudzio: {bg_panics} background-thread panic(s) detected outside any test boundary; \
             marking run as FAILED. Re-run with RUST_BACKTRACE=full for the panic location.\n"
        ));
        return ExitCode::from(1);
    }

    if grand_total.is_success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Format the `<runtime>` info block for events that don't carry an
/// elapsed or outcome (ignored / cancelled-before-dispatch).
fn runtime_only_info(runtime_name: &str) -> String {
    format!("<{runtime_name}>")
}

/// Build the padded, coloured status tag for an outcome.
/// Returns `(rendered_with_color, visible_width)`.
fn status_tag(outcome_label: StatusLabel, colored: bool) -> (String, usize) {
    let (word, code) = match outcome_label {
        StatusLabel::Ok => ("OK", "32"),
        StatusLabel::Fail => ("FAIL", "31"),
        StatusLabel::Panic => ("PANIC", "31"),
        StatusLabel::Timeout => ("TIMEOUT", "33"),
        StatusLabel::Ignore => ("IGNORE", "2"),
        StatusLabel::Cancel => ("CANCEL", "33"),
        StatusLabel::Bench => ("BENCH", "32"),
        StatusLabel::BenchErr => ("BENCH", "31"),
        StatusLabel::Setup => ("SETUP", "31"),
        StatusLabel::Hang => ("HANG", "31"),
    };
    let naked = format!("[{word}]");
    let visible = naked.chars().count();
    let painted = paint(&naked, code, colored);
    // Pad with trailing spaces so the status column has uniform width.
    let pad = STATUS_TAG_WIDTH.saturating_sub(visible);
    let mut out = painted;
    for _ in 0..pad {
        out.push(' ');
    }
    (out, STATUS_TAG_WIDTH)
}

/// Best-effort terminal column count for right-aligning the trailing
/// info. Falls back to 100 when we can't query the TTY.
#[cfg(unix)]
fn terminal_width() -> usize {
    use std::os::fd::AsRawFd as _;
    #[expect(
        unsafe_code,
        reason = "zero-initialised winsize; FFI; see SAFETY comment below"
    )]
    // SAFETY: zeroing a libc::winsize is safe — it's a plain C struct
    // of integers with no validity invariants beyond bit-pattern.
    let mut ws: libc::winsize = unsafe { mem::zeroed() };
    let fd = io::stdout().as_raw_fd();
    #[expect(
        unsafe_code,
        reason = "ioctl TIOCGWINSZ FFI call; see SAFETY comment below"
    )]
    // SAFETY: ioctl TIOCGWINSZ writes into the `winsize` we allocated
    // above; the pointer is properly aligned and exclusively owned.
    // Result is read only on success.
    let ioctl_ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut ws) };
    if ioctl_ret == 0_i32 && ws.ws_col > 0 {
        return usize::from(ws.ws_col);
    }
    100
}

#[cfg(not(unix))]
fn terminal_width() -> usize {
    100
}

/// Decide whether a single test should run, given the user-supplied
/// filter, `--skip` substrings, and `--ignored` / `--include-ignored`
/// mode.
///
/// `qualified_name` is the same string the runner displays in its
/// output (see [`qualified_test_name`]). Filter and skip both
/// substring-match against it, so anything a user can copy out of the
/// runner's output is a valid filter.
#[inline]
#[must_use]
pub fn token_passes_filters(
    qualified_name: &str,
    ignored: bool,
    filter: Option<&str>,
    skip_filters: &[String],
    run_ignored: RunIgnoredMode,
) -> bool {
    if let Some(needle) = filter
        && !qualified_name.contains(needle)
    {
        return false;
    }
    for skip in skip_filters {
        if qualified_name.contains(skip.as_str()) {
            return false;
        }
    }
    match run_ignored {
        RunIgnoredMode::Normal | RunIgnoredMode::Include => true,
        RunIgnoredMode::Only => ignored,
    }
}

/// Format the trailing `<...>` info block for an outcome. Runtime
/// name comes first, then elapsed, then bench-specific details.
fn trailing_info(outcome: &TestOutcome, runtime_name: &str) -> String {
    match outcome {
        TestOutcome::Passed { elapsed }
        | TestOutcome::Failed { elapsed, .. }
        | TestOutcome::Panicked { elapsed }
        | TestOutcome::Hung { elapsed }
        | TestOutcome::SetupFailed { elapsed, .. } => {
            format!("<{runtime_name}, {}>", format_elapsed(*elapsed))
        }
        TestOutcome::TimedOut | TestOutcome::Cancelled => format!("<{runtime_name}>"),
        TestOutcome::Benched { elapsed, report } => {
            let mut inner = format!(
                "{runtime_name}, {}, {}",
                format_elapsed(*elapsed),
                report.strategy,
            );
            if let Some(p50) = report.median() {
                let p50_text = fmt_duration(p50);
                let _write_ret: Result<(), fmt::Error> = write!(inner, ", p50 {p50_text}");
            }
            format!("<{inner}>")
        }
    }
}

/// Decide whether ANSI colour escapes should be emitted in plain
/// mode, honouring `--color`, `NO_COLOR`, and the stdout-TTY check.
fn use_color(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal(),
    }
}

/// Wrap `text` with the yellow ANSI SGR code when `colored` is true.
fn yellow(text: &str, colored: bool) -> String {
    paint(text, "33", colored)
}
