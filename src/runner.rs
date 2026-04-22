use std::collections::HashMap;
use std::env;
use std::io::{self, IsTerminal as _, Write as _};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::config::{ColorMode, Config, Format, RunIgnoredMode};
use crate::suite::{
    RuntimeGroupKey, RuntimeGroupOwner, SuiteReporter, SuiteRunRequest, SuiteSummary, TestOutcome,
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
    pub ignored: usize,
    pub panicked: usize,
    pub passed: usize,
    pub timed_out: usize,
    pub total: usize,
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
        self.failed == 0 && self.timed_out == 0 && self.panicked == 0 && self.cancelled == 0
    }

    #[inline]
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        Self {
            cancelled: self.cancelled.saturating_add(other.cancelled),
            failed: self.failed.saturating_add(other.failed),
            ignored: self.ignored.saturating_add(other.ignored),
            panicked: self.panicked.saturating_add(other.panicked),
            passed: self.passed.saturating_add(other.passed),
            timed_out: self.timed_out.saturating_add(other.timed_out),
            total: self.total.saturating_add(other.total),
        }
    }

    #[inline]
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            cancelled: 0,
            failed: 0,
            ignored: 0,
            panicked: 0,
            passed: 0,
            timed_out: 0,
            total: 0,
        }
    }
}

impl From<SuiteSummary> for TestSummary {
    #[inline]
    fn from(s: SuiteSummary) -> Self {
        Self {
            cancelled: s.cancelled,
            failed: s.failed,
            ignored: s.ignored,
            panicked: s.panicked,
            passed: s.passed,
            timed_out: s.timed_out,
            total: s.total,
        }
    }
}

// ---------------------------------------------------------------------------
// Color helpers
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
// Failure accumulator
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FailureInfo {
    name: &'static str,
    message: String,
}

// ---------------------------------------------------------------------------
// Default reporter
// ---------------------------------------------------------------------------

struct DefaultReporter {
    failures: Mutex<Vec<FailureInfo>>,
    fmt: Format,
    colored: bool,
}

impl SuiteReporter for DefaultReporter {
    fn report_ignored(&self, token: &'static TestToken, runtime_name: &'static str) {
        match self.fmt {
            Format::Terse => {
                print!("{}", yellow("i", self.colored));
                let _flush = io::stdout().flush();
            }
            Format::Pretty => {
                let label = yellow("ignored", self.colored);
                if token.ignore_reason.is_empty() {
                    println!("test {} [{}] ... {}", token.name, runtime_name, label);
                } else {
                    println!(
                        "test {} [{}] ... {}, {}",
                        token.name, runtime_name, label, token.ignore_reason
                    );
                }
            }
        }
    }

    fn report_cancelled(&self, token: &'static TestToken, runtime_name: &'static str) {
        match self.fmt {
            Format::Terse => {
                print!("{}", yellow("c", self.colored));
                let _flush = io::stdout().flush();
            }
            Format::Pretty => {
                println!(
                    "test {} [{}] ... {}",
                    token.name,
                    runtime_name,
                    yellow("cancelled", self.colored),
                );
            }
        }
    }

    fn report_outcome(
        &self,
        token: &'static TestToken,
        runtime_name: &'static str,
        outcome: TestOutcome,
    ) {
        match self.fmt {
            Format::Terse => {
                let ch = match &outcome {
                    TestOutcome::Passed { .. } => ".".to_owned(),
                    TestOutcome::Failed { .. }
                    | TestOutcome::Panicked { .. }
                    | TestOutcome::TimedOut => red("F", self.colored),
                    TestOutcome::Cancelled => yellow("c", self.colored),
                    TestOutcome::Benched { report, .. } => {
                        if report.is_success() {
                            "b".to_owned()
                        } else {
                            red("B", self.colored)
                        }
                    }
                };
                print!("{ch}");
                let _flush = io::stdout().flush();
            }
            Format::Pretty => {
                let status = match &outcome {
                    TestOutcome::Passed { elapsed } => {
                        format!("{} ({elapsed:.2?})", green("ok", self.colored))
                    }
                    TestOutcome::Failed { elapsed, .. } => {
                        format!("{} ({elapsed:.2?})", red("FAILED", self.colored))
                    }
                    TestOutcome::Panicked { elapsed } => {
                        format!("{} ({elapsed:.2?})", red("FAILED (panicked)", self.colored))
                    }
                    TestOutcome::TimedOut => red("FAILED (timed out)", self.colored),
                    TestOutcome::Cancelled => yellow("cancelled", self.colored),
                    TestOutcome::Benched { elapsed, report } => {
                        let label = if report.is_success() {
                            green("benched", self.colored)
                        } else {
                            red("benched (with errors)", self.colored)
                        };
                        format!("{label} ({elapsed:.2?})")
                    }
                };
                // For bench outcomes every line must be emitted as a
                // single `println!` so concurrent runtime threads don't
                // interleave each other's histograms mid-run. `println!`
                // takes the stdout lock once per call, so a single
                // buffer with embedded newlines prints atomically.
                let header = format!("test {} [{}] ... {}", token.name, runtime_name, status);
                if let TestOutcome::Benched { report, .. } = &outcome {
                    let mut buf = header;
                    buf.push_str(&format!(
                        "\n    strategy: {}  {}",
                        report.strategy,
                        report.summary_line(),
                    ));
                    if report.failures.is_empty() && report.panics == 0 {
                        let histogram = report.ascii_histogram(8, 30);
                        if !histogram.is_empty() {
                            buf.push('\n');
                            buf.push_str(histogram.trim_end_matches('\n'));
                        }
                    } else {
                        buf.push_str(&format!(
                            "\n    {} iterations failed, {} panicked",
                            report.failures.len(),
                            report.panics,
                        ));
                    }
                    println!("{buf}");
                } else {
                    println!("{header}");
                }
            }
        }

        match outcome {
            TestOutcome::Failed { message, .. } => {
                let mut guard = self
                    .failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.push(FailureInfo {
                    name: token.name,
                    message,
                });
            }
            TestOutcome::Benched { report, .. } if !report.is_success() => {
                let mut guard = self
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
}

// ---------------------------------------------------------------------------
// run()
// ---------------------------------------------------------------------------

/// Collect all registered [`TestToken`]s, group them by `runtime_group_key`,
/// run each group in its own OS thread via its
/// [`RuntimeGroupOwner`](crate::suite::RuntimeGroupOwner), print results in
/// cargo-test format, then exit the process.
///
/// `cargo` comes from the caller (the `#[rudzio::main]` macro expands
/// `cargo_meta!()` at the user's crate site so the `env!(...)` values
/// belong to that crate, not rudzio).
pub fn run(cargo: crate::config::CargoMeta) -> ! {
    let config = Config::parse(cargo);
    let colored = use_color(config.color);

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

    // --list: print test names and exit.
    if config.list {
        for token in &filtered_tokens {
            // --list runs before any group thread starts, so no runtime
            // exists yet to query its `name()`. Print just the test name.
            println!("{}: test", token.name);
        }
        std::process::exit(0);
    }

    let total_count = filtered_tokens.len();
    println!(
        "running {} {}",
        total_count,
        if total_count == 1 { "test" } else { "tests" }
    );

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

    // Group tokens by runtime_group_key (compile-time hash of the
    // (runtime, suite) path strings). Tokens that share a key share an OS
    // thread, a runtime instance, and a suite context — even when emitted
    // by different `#[rudzio::suite]` blocks.
    let mut groups: HashMap<RuntimeGroupKey, Vec<&'static TestToken>> = HashMap::new();
    for token in &filtered_tokens {
        groups
            .entry(token.runtime_group_key)
            .or_default()
            .push(token);
    }

    let reporter = Arc::new(DefaultReporter {
        failures: Mutex::new(Vec::new()),
        fmt: config.format,
        colored,
    });

    let start = Instant::now();

    // Share the resolved Config across every per-group thread. `Arc` keeps
    // it cheap; we hand out `&Config` to each runtime constructor.
    let config = Arc::new(config);

    let handles: Vec<_> = groups
        .into_values()
        .map(|mut group_tokens| {
            // Stable source order (file, line) across the whole group.
            group_tokens.sort_by_key(|t| (t.file, t.line));
            // All tokens in this group share `runtime_group_key`; their
            // `runtime_group_owner` pointers are functionally equivalent
            // (same R, same S constructors emitted by separate suite
            // blocks). Pick the first one to drive the group.
            let owner: &'static dyn RuntimeGroupOwner = group_tokens[0].runtime_group_owner;
            // Each group gets a CHILD of the run-wide root so that the
            // suite's teardown (which a user impl can validly cancel
            // wholesale) only fans out within this group, not across to
            // sibling groups still in-flight on other threads. SIGINT /
            // SIGTERM / --run-timeout still propagate because they cancel
            // the parent.
            let req_root = root_token.child_token();
            let reporter = Arc::clone(&reporter);
            let config = Arc::clone(&config);
            thread::spawn(move || {
                let req = SuiteRunRequest {
                    tokens: &group_tokens,
                    config: &config,
                    root_token: req_root,
                };
                owner.run_group(req, &*reporter)
            })
        })
        .collect();

    let total = handles
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
        });

    let elapsed = start.elapsed();

    if config.format == Format::Terse && total_count > 0 {
        println!();
    }

    let guard = reporter
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
        bold(&green("ok", colored), colored)
    } else {
        bold(&red("FAILED", colored), colored)
    };

    println!(
        "test result: {}. {} passed; {} failed; {} panicked; {} timed out; \
         {} cancelled; {} ignored; 0 measured; {} total; {} filtered out; \
         finished in {elapsed:.2?}",
        result_label,
        total.passed,
        total.failed,
        total.panicked,
        total.timed_out,
        total.cancelled,
        total.ignored,
        total.total,
        filtered_out,
    );

    std::process::exit(total.exit_code())
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
