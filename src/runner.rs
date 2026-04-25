use std::collections::HashMap;
use std::env;
use std::io::{self, IsTerminal as _, Write as _};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::config::{ColorMode, Config, Format, OutputMode, RunIgnoredMode};
use crate::output::events::LifecycleEvent;
use crate::output::{self};
use crate::suite::{
    RuntimeGroupKey, RuntimeGroupOwner, SuiteReporter, SuiteRunRequest, SuiteSummary,
    TeardownResult, TestOutcome,
};
use crate::token::{TEST_TOKENS, TestToken};

// ---------------------------------------------------------------------------
// TestSummary
// ---------------------------------------------------------------------------

/// Results of a test run.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct TestSummary {
    pub cancelled: usize,
    pub failed: usize,
    /// Tests escalated from `TimedOut` to `Hung` because they ignored
    /// cooperative cancellation past `--phase-hang-grace`. Counted
    /// alongside `failed`/`panicked`/`timed_out` for `is_success`.
    pub hung: usize,
    pub ignored: usize,
    pub panicked: usize,
    pub passed: usize,
    pub timed_out: usize,
    pub total: usize,
    /// Combined count of suite-level + per-test teardown failures
    /// (Err *or* panic *or* Hung). Drives [`is_success`] so a botched
    /// cleanup fails the run even when every test body passed.
    pub teardown_failures: usize,
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
    fn from(s: SuiteSummary) -> Self {
        Self {
            cancelled: s.cancelled,
            failed: s.failed,
            hung: s.hung,
            ignored: s.ignored,
            panicked: s.panicked,
            passed: s.passed,
            timed_out: s.timed_out,
            total: s.total,
            teardown_failures: s.teardown_failures,
        }
    }
}

// ---------------------------------------------------------------------------
// Color helpers (used by the Plain-mode reporter only; live mode has
// its own ColorPolicy in src/output/color.rs).
// ---------------------------------------------------------------------------

fn use_color(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal(),
    }
}

fn paint(s: &str, code: &str, colored: bool) -> String {
    if colored {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

fn green(s: &str, c: bool) -> String {
    paint(s, "32", c)
}
fn red(s: &str, c: bool) -> String {
    paint(s, "31", c)
}
fn yellow(s: &str, c: bool) -> String {
    paint(s, "33", c)
}
fn bold(s: &str, c: bool) -> String {
    paint(s, "1", c)
}

// ---------------------------------------------------------------------------
// New-format rendering helpers (used by both plain and live modes)
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

/// Max visible width (brackets inclusive) of any status label we emit,
/// used to pad so everything after lines up.
const STATUS_TAG_WIDTH: usize = 9; // `[TIMEOUT]` / `[CANCEL] ` / `[IGNORE] `

/// Minimum column padding between name and `<...>` info block when
/// nothing forces a wider layout.
const MIN_TRAILING_PAD: usize = 2;

/// Best-effort terminal column count for right-aligning the trailing
/// info. Falls back to 100 when we can't query the TTY.
#[cfg(unix)]
fn terminal_width() -> usize {
    use std::os::fd::AsRawFd as _;
    // SAFETY: ioctl TIOCGWINSZ writes a `winsize` struct; we supply a
    // zero-initialised one and only read it when ioctl returns 0.
    #[allow(unsafe_code)]
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        let fd = io::stdout().as_raw_fd();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut ws) == 0 && ws.ws_col > 0 {
            return usize::from(ws.ws_col);
        }
    }
    100
}

#[cfg(not(unix))]
fn terminal_width() -> usize {
    100
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
    Setup,
    Hang,
}

impl StatusLabel {
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
                inner.push_str(&format!(", p50 {p50:.2?}"));
            }
            format!("<{inner}>")
        }
    }
}

/// Format the `<runtime>` info block for events that don't carry an
/// elapsed or outcome (ignored / cancelled-before-dispatch).
fn runtime_only_info(runtime_name: &str) -> String {
    format!("<{runtime_name}>")
}

/// One-shot diagnostic message for a finished test — rendered
/// indented right under its status line so the reason for failure
/// (test body error, setup error, panic payload, timeout note)
/// is visible without scrolling to the end-of-run failures section.
fn outcome_inline_message(outcome: &TestOutcome) -> Option<String> {
    match outcome {
        TestOutcome::Failed { message, .. } => Some(message.clone()),
        TestOutcome::SetupFailed { message, .. } => {
            Some(format!("test setup failed: {message}"))
        }
        TestOutcome::TimedOut => Some("test exceeded its timeout".to_owned()),
        TestOutcome::Hung { .. } => Some("hung; abort signal sent".to_owned()),
        TestOutcome::Cancelled => {
            Some("test was cancelled before completion".to_owned())
        }
        TestOutcome::Panicked { .. }
        | TestOutcome::Passed { .. }
        | TestOutcome::Benched { .. } => None,
    }
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
    let mut out = String::with_capacity(lhs_rendered.len() + pad + trailing.len());
    out.push_str(lhs_rendered);
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(trailing);
    out
}

fn format_elapsed(d: Duration) -> String {
    format!("{d:.2?}")
}

// ---------------------------------------------------------------------------
// Failure accumulator (plain-mode only)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FailureInfo {
    name: &'static str,
    message: String,
}

// ---------------------------------------------------------------------------
// Mode-aware reporter
// ---------------------------------------------------------------------------

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
    plain: Option<PlainState>,
    /// Count of per-test teardown failures (Err or panic). The codegen
    /// calls [`Self::report_test_teardown_failure`] from inside each
    /// test's dispatch fn, where the per-thread `SuiteSummary` is not
    /// in scope; this atomic lets the runner fold them into the
    /// final `TestSummary.teardown_failures` so the run exits non-zero
    /// when any per-test teardown failed.
    test_teardown_failures: AtomicUsize,
}

struct PlainState {
    failures: Mutex<Vec<FailureInfo>>,
    fmt: Format,
    colored: bool,
}

impl ModeReporter {
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

impl SuiteReporter for ModeReporter {
    fn report_ignored(&self, token: &'static TestToken, runtime_name: &'static str) {
        if let Some(p) = &self.plain {
            match p.fmt {
                Format::Terse => {
                    print!("{}", yellow("i", p.colored));
                    let _flush = io::stdout().flush();
                }
                Format::Pretty => {
                    let (tag_rendered, tag_visible) = status_tag(StatusLabel::Ignore, p.colored);
                    let display = format!("{}::{}", token.module_path, token.name);
                    let trailing = if token.ignore_reason.is_empty() {
                        runtime_only_info(runtime_name)
                    } else {
                        format!("<{runtime_name}, {}>", token.ignore_reason)
                    };
                    let lhs_naked = format!("{:width$} {display}", "", width = tag_visible,);
                    let lhs_rendered = format!("{tag_rendered} {display}");
                    let line =
                        render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                    println!("{line}");
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

    fn report_cancelled(&self, token: &'static TestToken, runtime_name: &'static str) {
        if let Some(p) = &self.plain {
            match p.fmt {
                Format::Terse => {
                    print!("{}", yellow("c", p.colored));
                    let _flush = io::stdout().flush();
                }
                Format::Pretty => {
                    let (tag_rendered, tag_visible) = status_tag(StatusLabel::Cancel, p.colored);
                    let display = format!("{}::{}", token.module_path, token.name);
                    let lhs_naked = format!("{:width$} {display}", "", width = tag_visible,);
                    let lhs_rendered = format!("{tag_rendered} {display}");
                    let trailing = runtime_only_info(runtime_name);
                    let line =
                        render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                    println!("{line}");
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

    fn report_outcome(
        &self,
        token: &'static TestToken,
        runtime_name: &'static str,
        outcome: TestOutcome,
    ) {
        let Some(p) = &self.plain else {
            // Live mode: macro-generated dispatch already emitted
            // TestCompleted with the full outcome; nothing to do.
            return;
        };
        match p.fmt {
            Format::Terse => {
                let ch = match &outcome {
                    TestOutcome::Passed { .. } => ".".to_owned(),
                    TestOutcome::Failed { .. }
                    | TestOutcome::Panicked { .. }
                    | TestOutcome::SetupFailed { .. }
                    | TestOutcome::TimedOut
                    | TestOutcome::Hung { .. } => red("F", p.colored),
                    TestOutcome::Cancelled => yellow("c", p.colored),
                    TestOutcome::Benched { report, .. } => {
                        if report.is_success() {
                            "b".to_owned()
                        } else {
                            red("B", p.colored)
                        }
                    }
                };
                print!("{ch}");
                let _flush = io::stdout().flush();
            }
            Format::Pretty => {
                let label = StatusLabel::from_outcome(&outcome);
                let (tag_rendered, tag_visible) = status_tag(label, p.colored);
                let display = format!("{}::{}", token.module_path, token.name);
                let trailing = trailing_info(&outcome, runtime_name);
                let lhs_naked = format!("{:width$} {display}", "", width = tag_visible,);
                let lhs_rendered = format!("{tag_rendered} {display}");
                let header =
                    render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                if let TestOutcome::Benched { report, .. } = &outcome {
                    // Bench status line + detailed stats + histogram,
                    // emitted as a single atomic println! so concurrent
                    // runtime threads can't interleave each other's
                    // blocks.
                    let mut buf = header;
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
                    println!("{buf}");
                } else {
                    // Single atomic write: header + inlined failure
                    // message (if any), rendered in the tag's color
                    // for failing outcomes so the reason is visible
                    // alongside the status line without scrolling to
                    // the end-of-run failures section.
                    let mut buf = header;
                    if let Some(msg) = outcome_inline_message(&outcome) {
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
                                red(&body, p.colored)
                            } else {
                                body
                            };
                            buf.push_str(&painted);
                        }
                    }
                    println!("{buf}");
                }
            }
        }

        match outcome {
            TestOutcome::Failed { message, .. } => {
                let mut guard = p
                    .failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.push(FailureInfo {
                    name: token.name,
                    message,
                });
            }
            TestOutcome::SetupFailed { message, .. } => {
                let mut guard = p
                    .failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.push(FailureInfo {
                    name: token.name,
                    message: format!("test setup failed: {message}"),
                });
            }
            TestOutcome::Benched { report, .. } if !report.is_success() => {
                let mut guard = p
                    .failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let message = format!(
                    "benchmark {} reported {} failed iterations and {} panics:\n{}",
                    report.strategy,
                    report.failures.len(),
                    report.panics,
                    report.failures.join("\n"),
                );
                guard.push(FailureInfo {
                    name: token.name,
                    message,
                });
            }
            _ => {}
        }
    }

    fn report_warning(&self, message: &str) {
        eprintln!("warning: {message}");
    }

    fn report_suite_setup_started(&self, runtime_name: &'static str, suite: &'static str) {
        if let Some(p) = &self.plain {
            if matches!(p.fmt, Format::Pretty) {
                println!("setup    {suite} ... started <{runtime_name}>");
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

    fn report_suite_setup_finished(
        &self,
        runtime_name: &'static str,
        suite: &'static str,
        elapsed: Duration,
        error: Option<&str>,
    ) {
        if let Some(p) = &self.plain {
            if matches!(p.fmt, Format::Pretty) {
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
                let (tag_rendered, tag_visible) = status_tag(label, p.colored);
                let display = format!("setup {suite}");
                let trailing = format!("<{runtime_name}, {}>", format_elapsed(elapsed));
                let lhs_naked = format!("{:width$} {display}", "", width = tag_visible);
                let lhs_rendered = format!("{tag_rendered} {display}");
                let line =
                    render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                println!("{line}");
                if let Some(msg) = error {
                    println!("  {}", red(&format!("error: {msg}"), p.colored));
                }
            }
            if let Some(msg) = error {
                let mut guard = p
                    .failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.push(FailureInfo {
                    name: "<suite setup>",
                    message: format!("setup {suite} [{runtime_name}]: {msg}"),
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

    fn report_suite_teardown_started(&self, runtime_name: &'static str, suite: &'static str) {
        if let Some(p) = &self.plain {
            if matches!(p.fmt, Format::Pretty) {
                println!("teardown {suite} ... started <{runtime_name}>");
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

    fn report_suite_teardown_finished(
        &self,
        runtime_name: &'static str,
        suite: &'static str,
        elapsed: Duration,
        result: TeardownResult,
    ) {
        if let Some(p) = &self.plain {
            if matches!(p.fmt, Format::Pretty) {
                let label = match result {
                    TeardownResult::Ok => StatusLabel::Ok,
                    TeardownResult::Err(_) => StatusLabel::Fail,
                    TeardownResult::Panicked(_) => StatusLabel::Panic,
                    TeardownResult::TimedOut => StatusLabel::Timeout,
                    TeardownResult::Hung => StatusLabel::Hang,
                };
                let (tag_rendered, tag_visible) = status_tag(label, p.colored);
                let display = format!("teardown {suite}");
                let trailing = format!("<{runtime_name}, {}>", format_elapsed(elapsed));
                let lhs_naked = format!("{:width$} {display}", "", width = tag_visible);
                let lhs_rendered = format!("{tag_rendered} {display}");
                let line =
                    render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                println!("{line}");
                match &result {
                    TeardownResult::Ok => {}
                    TeardownResult::Err(msg) => {
                        println!("  {}", red(&format!("error: {msg}"), p.colored));
                    }
                    TeardownResult::Panicked(msg) => {
                        println!("  {}", red(&format!("panic: {msg}"), p.colored));
                    }
                    TeardownResult::TimedOut => {
                        println!("  {}", red("timeout: teardown timed out", p.colored));
                    }
                    TeardownResult::Hung => {
                        println!(
                            "  {}",
                            red("hang: teardown hung; abort signal sent", p.colored)
                        );
                    }
                }
            }
            match result {
                TeardownResult::Ok => {}
                TeardownResult::Err(msg) => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
                        name: "<suite teardown>",
                        message: format!("teardown {suite} [{runtime_name}]: {msg}"),
                    });
                }
                TeardownResult::Panicked(msg) => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
                        name: "<suite teardown>",
                        message: format!("teardown {suite} [{runtime_name}]: panic: {msg}"),
                    });
                }
                TeardownResult::TimedOut => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
                        name: "<suite teardown>",
                        message: format!(
                            "teardown {suite} [{runtime_name}]: timeout: teardown timed out"
                        ),
                    });
                }
                TeardownResult::Hung => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
                        name: "<suite teardown>",
                        message: format!(
                            "teardown {suite} [{runtime_name}]: hang: teardown hung; abort signal sent"
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

    fn report_test_teardown_failure(
        &self,
        token: &'static TestToken,
        runtime_name: &'static str,
        result: TeardownResult,
    ) {
        if !matches!(result, TeardownResult::Ok) {
            let _prev = self.test_teardown_failures.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(p) = &self.plain {
            let display = format!("{}::{}", token.module_path, token.name);
            if matches!(p.fmt, Format::Pretty) {
                let label = match result {
                    TeardownResult::Ok => return,
                    TeardownResult::Err(_) => StatusLabel::Fail,
                    TeardownResult::Panicked(_) => StatusLabel::Panic,
                    TeardownResult::TimedOut => StatusLabel::Timeout,
                    TeardownResult::Hung => StatusLabel::Hang,
                };
                let (tag_rendered, tag_visible) = status_tag(label, p.colored);
                let lhs_display = format!("teardown {display}");
                let trailing = format!("<{runtime_name}>");
                let lhs_naked = format!("{:width$} {lhs_display}", "", width = tag_visible);
                let lhs_rendered = format!("{tag_rendered} {lhs_display}");
                let line =
                    render_status_line(&lhs_naked, &lhs_rendered, &trailing, terminal_width());
                println!("{line}");
                match &result {
                    TeardownResult::Ok => {}
                    TeardownResult::Err(msg) => {
                        println!("  {}", red(&format!("error: {msg}"), p.colored));
                    }
                    TeardownResult::Panicked(msg) => {
                        println!("  {}", red(&format!("panic: {msg}"), p.colored));
                    }
                    TeardownResult::TimedOut => {
                        println!("  {}", red("timeout: teardown timed out", p.colored));
                    }
                    TeardownResult::Hung => {
                        println!(
                            "  {}",
                            red("hang: teardown hung; abort signal sent", p.colored)
                        );
                    }
                }
            }
            match result {
                TeardownResult::Ok => {}
                TeardownResult::Err(msg) => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
                        name: token.name,
                        message: format!("test teardown failed [{runtime_name}]: {msg}"),
                    });
                }
                TeardownResult::Panicked(msg) => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
                        name: token.name,
                        message: format!("test teardown panicked [{runtime_name}]: {msg}"),
                    });
                }
                TeardownResult::TimedOut => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
                        name: token.name,
                        message: format!(
                            "test teardown timed out [{runtime_name}]: teardown timed out"
                        ),
                    });
                }
                TeardownResult::Hung => {
                    let mut guard = p
                        .failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(FailureInfo {
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
}

// ---------------------------------------------------------------------------
// run()
// ---------------------------------------------------------------------------

/// Collect all registered [`TestToken`]s, group them by
/// `runtime_group_key`, run each group in its own OS thread via its
/// [`RuntimeGroupOwner`](crate::suite::RuntimeGroupOwner), and render
/// per-test output according to [`Config::output_mode`]:
///
/// - [`OutputMode::Live`]: the bottom-of-terminal live region + append
///   history (see `crate::output::render`).
/// - [`OutputMode::Plain`]: classic cargo-test-style lines; the
///   runner prints them directly from its reporter.
///
/// `cargo` comes from the caller (the `#[rudzio::main]` macro expands
/// `cargo_meta!()` at the user's crate site so the `env!(...)` values
/// belong to that crate, not rudzio).
pub fn run(cargo: crate::config::CargoMeta) -> ! {
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
        print!("{}", crate::config::USAGE);
        process::exit(0);
    }

    let colored_plain = matches!(config.output_mode, OutputMode::Plain) && use_color(config.color);

    // Output capture + render. In Plain mode returns a no-op guard
    // and the reporter below prints directly. In Live mode returns
    // the real drawer-driving guard.
    let capture_guard = match output::init(&config) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("rudzio: failed to initialise output capture: {e}");
            process::exit(2);
        }
    };

    let all_tokens: Vec<&'static TestToken> = TEST_TOKENS.iter().collect();

    let filtered_tokens: Vec<&'static TestToken> = all_tokens
        .iter()
        .copied()
        .filter(|t| {
            if let Some(ref f) = config.filter {
                if !t.name.contains(f.as_str()) {
                    return false;
                }
            }
            for skip in &config.skip_filters {
                if t.name.contains(skip.as_str()) {
                    return false;
                }
            }
            match config.run_ignored {
                RunIgnoredMode::Normal | RunIgnoredMode::Include => true,
                RunIgnoredMode::Only => t.ignored,
            }
        })
        .collect();

    let filtered_out = all_tokens.len().saturating_sub(filtered_tokens.len());

    if config.list {
        drop(capture_guard);
        for token in &filtered_tokens {
            println!("{}: test", token.name);
        }
        process::exit(0);
    }

    let total_count = filtered_tokens.len();
    // Only print the "running N tests" header in Plain mode — the
    // Live drawer has its own banner inside the live region.
    if matches!(config.output_mode, OutputMode::Plain) {
        println!(
            "running {} {}",
            total_count,
            if total_count == 1 { "test" } else { "tests" }
        );
    }

    // Root cancellation token: cancelled on run-timeout, SIGINT, or SIGTERM.
    let root_token = CancellationToken::new();
    install_signal_handler(root_token.clone());

    if let Some(dur) = config.run_timeout {
        let watchdog_token = root_token.clone();
        let _watchdog = thread::spawn(move || {
            thread::sleep(dur);
            if !watchdog_token.is_cancelled() {
                eprintln!("\nrun timeout ({dur:.2?}) exceeded, cancelling run...");
                watchdog_token.cancel();
            }
        });
    }

    // Layer-1 process-exit watchdog. Listens for `root_token`
    // cancellation (SIGINT / SIGTERM / --run-timeout / explicit
    // user cancel) and, after `--cancel-grace-period`, force-exits
    // the process with code 2. This is the universal safety net for
    // sync-blocked tasks that ignore every cooperative cancellation
    // signal (e.g. `std::thread::sleep` inside a test body) — no
    // amount of token-listening or `JoinHandle::abort()` will free
    // the worker, but `process::exit` lets the OS reap every thread.
    if let Some(grace) = config.cancel_grace_period {
        let watchdog_token = root_token.clone();
        let _watchdog = thread::Builder::new()
            .name("rudzio-cancel-grace-watchdog".to_owned())
            .spawn(move || {
                // Sync poll-loop until the token is cancelled. Avoids
                // an executor dep — the watchdog runs no rudzio test
                // code itself, just times out and force-exits. 50ms
                // tick is fine: this is fault-tolerance plumbing, not
                // a hot path, and 50ms latency before grace starts is
                // negligible against the multi-second grace itself.
                while !watchdog_token.is_cancelled() {
                    thread::sleep(Duration::from_millis(50));
                }
                thread::sleep(grace);
                eprintln!(
                    "\nrudzio: {grace:.2?} grace period exceeded after cancellation, \
                     force-exiting (some phase ignored cooperative cancel)"
                );
                process::exit(2);
            });
    }

    let mut groups: HashMap<RuntimeGroupKey, Vec<&'static TestToken>> = HashMap::new();
    for token in &filtered_tokens {
        groups
            .entry(token.runtime_group_key)
            .or_default()
            .push(token);
    }

    let reporter = ModeReporter::new(&config);
    let start = Instant::now();

    // Scoped group-dispatch: one OS thread per (runtime, suite) group,
    // all joined at scope exit. Lets each thread borrow `&config` and
    // `&reporter` directly instead of each one cloning an `Arc` — aligned
    // with the codebase rule against `'static` substitution where stack
    // borrows suffice.
    let total = thread::scope(|scope| {
        let handles: Vec<_> = groups
            .into_values()
            .map(|mut group_tokens| {
                group_tokens.sort_by_key(|t| (t.file, t.line));
                let owner: &'static dyn RuntimeGroupOwner = group_tokens[0].runtime_group_owner;
                let req_root = root_token.child_token();
                let config_ref = &config;
                let reporter_ref = &reporter;
                scope.spawn(move || {
                    let req = SuiteRunRequest {
                        tokens: &group_tokens,
                        config: config_ref,
                        root_token: req_root,
                    };
                    owner.run_group(req, reporter_ref)
                })
            })
            .collect();

        handles
            .into_iter()
            .fold(TestSummary::zero(), |acc, handle| match handle.join() {
                Ok(suite_summary) => acc.merge(TestSummary::from(suite_summary)),
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("unknown panic");
                    eprintln!("error: runtime thread panicked: {msg}");
                    acc.merge(TestSummary {
                        panicked: 1,
                        total: 1,
                        ..TestSummary::zero()
                    })
                }
            })
    });

    // Per-test teardown failures aren't visible to the per-thread
    // SuiteSummary (the per-test fn doesn't have it in scope), so fold
    // the reporter's atomic counter in here.
    let total = total.merge(TestSummary {
        teardown_failures: reporter.test_teardown_failures.load(Ordering::Relaxed),
        ..TestSummary::zero()
    });

    let elapsed = start.elapsed();

    // Plain-mode summary rendering. Live mode lets the drawer handle
    // it during its shutdown path.
    if let Some(p) = &reporter.plain {
        if p.fmt == Format::Terse && total_count > 0 {
            println!();
        }
        let guard = p
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.is_empty() {
            println!("\nfailures:\n");
            for f in guard.iter() {
                println!("---- {} ----", f.name);
                println!("{}\n", f.message);
            }
            println!("failures:");
            for f in guard.iter() {
                println!("    {}", f.name);
            }
            println!();
        }
        drop(guard);

        let result_label = if total.is_success() {
            bold(&green("ok", colored_plain), colored_plain)
        } else {
            bold(&red("FAILED", colored_plain), colored_plain)
        };

        println!(
            "test result: {}. {} passed; {} failed; {} panicked; {} timed out; \
             {} cancelled; {} ignored; {} teardown failed; 0 measured; {} total; \
             {} filtered out; finished in {elapsed:.2?}",
            result_label,
            total.passed,
            total.failed,
            total.panicked,
            total.timed_out,
            total.cancelled,
            total.ignored,
            total.teardown_failures,
            total.total,
            filtered_out,
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
    if bg_panics > 0 && total.is_success() {
        eprintln!(
            "rudzio: {bg_panics} background-thread panic(s) detected outside any test boundary; \
             marking run as FAILED. Re-run with RUST_BACKTRACE=full for the panic location."
        );
        process::exit(1);
    }

    process::exit(total.exit_code())
}

/// Set `RUST_BACKTRACE=full` and `RUST_LIB_BACKTRACE=full` if neither
/// is already in the env, so panic messages always carry an actionable
/// backtrace under the rudzio runner. When the binary is launched
/// directly (no rudzio runner), this fn is never called and stdlib's
/// normal env-var lookup applies.
fn enable_full_backtrace_default() {
    // SAFETY: rudzio's entry point runs before any test threads spawn,
    // so no other thread is mutating the environment concurrently.
    // `env::set_var` is sound under that single-threaded invariant.
    #[allow(unsafe_code)]
    unsafe {
        if env::var_os("RUST_BACKTRACE").is_none() {
            env::set_var("RUST_BACKTRACE", "full");
        }
        if env::var_os("RUST_LIB_BACKTRACE").is_none() {
            env::set_var("RUST_LIB_BACKTRACE", "full");
        }
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn install_signal_handler(token: CancellationToken) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("warning: failed to install signal handler: {err}");
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
                eprintln!("\nreceived {name}, cancelling run...");
                token.cancel();
            }
        });
}

#[cfg(not(unix))]
fn install_signal_handler(_token: CancellationToken) {}
