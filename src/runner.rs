use std::collections::HashMap;
use std::env;
use std::io::{self, IsTerminal as _, Write as _};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::suite::{
    RunIgnoredMode, RuntimeGroupKey, RuntimeGroupOwner, SuiteReporter, SuiteRunRequest,
    SuiteSummary, TestOutcome,
};
use crate::token::{TestToken, TEST_TOKENS};

// ---------------------------------------------------------------------------
// RunConfig (public API, kept for advanced use)
// ---------------------------------------------------------------------------

/// Configuration for a test run.
#[non_exhaustive]
#[derive(Debug)]
pub struct RunConfig {
    /// Cancellation token to abort the run.
    pub cancel: CancellationToken,
    /// Optional substring filter for test names.
    pub filter: Option<String>,
    /// Maximum number of concurrently running tests per runtime group.
    pub threads: usize,
    /// Per-test timeout.
    pub timeout: Duration,
}

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
// CLI args
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Pretty,
    Terse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug)]
struct CliArgs {
    filter: Option<String>,
    skip_filters: Vec<String>,
    threads: usize,
    format: Format,
    color: ColorMode,
    run_ignored: RunIgnoredMode,
    list: bool,
    /// Per-test timeout. `None` = no limit.
    test_timeout: Option<Duration>,
    /// Total-run timeout. `None` = no limit.
    run_timeout: Option<Duration>,
}

fn parse_cli_args() -> CliArgs {
    let mut filter: Option<String> = None;
    let mut skip_filters: Vec<String> = Vec::new();
    let mut threads: Option<usize> = None;
    let mut format = Format::Pretty;
    let mut color = ColorMode::Auto;
    let mut run_ignored = RunIgnoredMode::Normal;
    let mut list = false;
    let mut test_timeout: Option<Duration> = None;
    let mut run_timeout: Option<Duration> = None;

    let argv: Vec<String> = env::args().skip(1).collect();
    let mut i = 0_usize;
    while i < argv.len() {
        let arg = &argv[i];

        if let Some(rest) = arg.strip_prefix("--test-threads=") {
            if let Ok(n) = rest.parse::<usize>() && n > 0 {
                threads = Some(n);
            }
        } else if arg == "--test-threads" {
            i += 1;
            if let Some(next) = argv.get(i)
                && let Ok(n) = next.parse::<usize>()
                && n > 0
            {
                threads = Some(n);
            }
        } else if let Some(rest) = arg.strip_prefix("--color=") {
            color = match rest {
                "always" => ColorMode::Always,
                "never" => ColorMode::Never,
                _ => ColorMode::Auto,
            };
        } else if arg == "--color" {
            i += 1;
            if let Some(next) = argv.get(i) {
                color = match next.as_str() {
                    "always" => ColorMode::Always,
                    "never" => ColorMode::Never,
                    _ => ColorMode::Auto,
                };
            }
        } else if let Some(rest) = arg.strip_prefix("--format=") {
            format = if rest == "terse" { Format::Terse } else { Format::Pretty };
        } else if arg == "--format" {
            i += 1;
            if argv.get(i).map_or(false, |s| s == "terse") {
                format = Format::Terse;
            }
        } else if arg == "--ignored" {
            run_ignored = RunIgnoredMode::Only;
        } else if arg == "--include-ignored" {
            run_ignored = RunIgnoredMode::Include;
        } else if arg == "--list" {
            list = true;
        } else if let Some(rest) = arg.strip_prefix("--test-timeout=") {
            if let Ok(secs) = rest.parse::<u64>() {
                test_timeout = Some(Duration::from_secs(secs));
            }
        } else if arg == "--test-timeout" {
            i += 1;
            if let Some(next) = argv.get(i)
                && let Ok(secs) = next.parse::<u64>()
            {
                test_timeout = Some(Duration::from_secs(secs));
            }
        } else if let Some(rest) = arg.strip_prefix("--run-timeout=") {
            if let Ok(secs) = rest.parse::<u64>() {
                run_timeout = Some(Duration::from_secs(secs));
            }
        } else if arg == "--run-timeout" {
            i += 1;
            if let Some(next) = argv.get(i)
                && let Ok(secs) = next.parse::<u64>()
            {
                run_timeout = Some(Duration::from_secs(secs));
            }
        } else if let Some(rest) = arg.strip_prefix("--skip=") {
            skip_filters.push(rest.to_owned());
        } else if arg == "--skip" {
            i += 1;
            if let Some(next) = argv.get(i) {
                skip_filters.push(next.clone());
            }
        }
        // positional: name filter (not a flag)
        else if !arg.starts_with('-') {
            filter = Some(arg.clone());
        }
        // all other --flags (--nocapture, --show-output, --quiet, etc.) are silently ignored

        i += 1;
    }

    let threads = threads
        .or_else(|| {
            env::var("RUST_TEST_THREADS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
        })
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZeroUsize::get));

    CliArgs { filter, skip_filters, threads, format, color, run_ignored, list, test_timeout, run_timeout }
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
    if colored { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_owned() }
}

fn green(s: &str, c: bool) -> String { paint(s, "32", c) }
fn red(s: &str, c: bool) -> String { paint(s, "31", c) }
fn yellow(s: &str, c: bool) -> String { paint(s, "33", c) }
fn bold(s: &str, c: bool) -> String { paint(s, "1", c) }

// ---------------------------------------------------------------------------
// Failure accumulator
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FailureInfo {
    name: &'static str,
    message: String,
}

// ---------------------------------------------------------------------------
// resolve_test_threads (kept for callers)
// ---------------------------------------------------------------------------

fn resolve_from<I>(argv: I, env_var: Option<&str>) -> Option<usize>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix("--test-threads=") {
            if let Ok(parsed) = rest.parse::<usize>() && parsed > 0 {
                return Some(parsed);
            }
        } else if arg == "--test-threads"
            && let Some(next) = iter.next()
            && let Ok(parsed) = next.parse::<usize>()
            && parsed > 0
        {
            return Some(parsed);
        }
    }
    if let Some(value) = env_var
        && let Ok(parsed) = value.parse::<usize>()
        && parsed > 0
    {
        return Some(parsed);
    }
    None
}

/// Resolve the maximum number of tests to run concurrently, matching
/// libtest's `--test-threads` semantics.
#[inline]
#[must_use]
pub fn resolve_test_threads() -> usize {
    let env_var = env::var("RUST_TEST_THREADS").ok();
    resolve_from(env::args().skip(1), env_var.as_deref())
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZeroUsize::get))
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
                };
                println!("test {} [{}] ... {}", token.name, runtime_name, status);
            }
        }

        if let TestOutcome::Failed { message, .. } = outcome {
            let mut guard = self.failures.lock().expect("failures mutex poisoned");
            guard.push(FailureInfo { name: token.name, message });
        }
    }

    fn report_warning(&self, message: &str) {
        eprintln!("warning: {message}");
    }
}

// ---------------------------------------------------------------------------
// run()
// ---------------------------------------------------------------------------

/// Collect all registered [`TestToken`]s, group them by `suite_id`, run each
/// suite in its own OS thread via its [`SuiteRunner`], print results in
/// cargo-test format, then exit the process.
pub fn run() -> ! {
    let args = parse_cli_args();
    let colored = use_color(args.color);

    let all_tokens: Vec<&'static TestToken> = TEST_TOKENS.iter().collect();

    let filtered_tokens: Vec<&'static TestToken> = all_tokens
        .iter()
        .copied()
        .filter(|t| {
            if let Some(ref f) = args.filter {
                if !t.name.contains(f.as_str()) {
                    return false;
                }
            }
            for skip in &args.skip_filters {
                if t.name.contains(skip.as_str()) {
                    return false;
                }
            }
            match args.run_ignored {
                RunIgnoredMode::Normal | RunIgnoredMode::Include => true,
                RunIgnoredMode::Only => t.ignored,
            }
        })
        .collect();

    let filtered_out = all_tokens.len().saturating_sub(filtered_tokens.len());

    // --list: print test names and exit.
    if args.list {
        for token in &filtered_tokens {
            println!("{} [{}]: test", token.name, token.runtime_name);
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

    if let Some(dur) = args.run_timeout {
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
    // (runtime, global) path strings). Tokens that share a key share an OS
    // thread, a runtime instance, and a global context — even when emitted
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
        fmt: args.format,
        colored,
    });

    let start = Instant::now();

    let handles: Vec<_> = groups
        .into_values()
        .map(|mut group_tokens| {
            // Stable source order (file, line) across the whole group.
            group_tokens.sort_by_key(|t| (t.file, t.line));
            // All tokens in this group share `runtime_group_key`; their
            // `runtime_group_owner` pointers are functionally equivalent
            // (same R, same G constructors emitted by separate suite
            // blocks). Pick the first one to drive the group.
            let owner: &'static dyn RuntimeGroupOwner = group_tokens[0].runtime_group_owner;
            let req_threads = args.threads;
            let req_timeout = args.test_timeout;
            let req_run_ignored = args.run_ignored;
            // Each group gets a CHILD of the run-wide root so that the
            // global's teardown (which a user impl can validly cancel
            // wholesale) only fans out within this group, not across to
            // sibling groups still in-flight on other threads. SIGINT /
            // SIGTERM / --run-timeout still propagate because they cancel
            // the parent.
            let req_root = root_token.child_token();
            let reporter = Arc::clone(&reporter);
            thread::spawn(move || {
                let req = SuiteRunRequest {
                    tokens: &group_tokens,
                    threads: req_threads,
                    test_timeout: req_timeout,
                    run_ignored: req_run_ignored,
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
                acc.merge(TestSummary { panicked: 1, total: 1, ..TestSummary::zero() })
            }
        });

    let elapsed = start.elapsed();

    if args.format == Format::Terse && total_count > 0 {
        println!();
    }

    let guard = reporter.failures.lock().expect("failures mutex poisoned");
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{resolve_from, resolve_test_threads};

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn joined_argv_form_is_parsed() {
        assert_eq!(resolve_from(argv(&["--test-threads=4"]), None), Some(4));
    }

    #[test]
    fn split_argv_form_is_parsed() {
        assert_eq!(resolve_from(argv(&["--test-threads", "8"]), None), Some(8));
    }

    #[test]
    fn env_var_alone_is_used() {
        assert_eq!(resolve_from(argv(&[]), Some("3")), Some(3));
    }

    #[test]
    fn argv_takes_precedence_over_env() {
        assert_eq!(resolve_from(argv(&["--test-threads=2"]), Some("7")), Some(2));
    }

    #[test]
    fn zero_falls_through_to_next_source() {
        assert_eq!(resolve_from(argv(&["--test-threads=0"]), Some("0")), None);
    }

    #[test]
    fn garbage_falls_through_to_next_source() {
        assert_eq!(resolve_from(argv(&["--test-threads=abc"]), Some("xyz")), None);
    }

    #[test]
    fn zero_in_env_is_ignored_when_argv_is_valid() {
        assert_eq!(resolve_from(argv(&["--test-threads=5"]), Some("0")), Some(5));
    }

    #[test]
    fn unknown_flags_are_ignored() {
        assert_eq!(
            resolve_from(
                argv(&["--nocapture", "--color=always", "--test-threads=3", "--format=json"]),
                None,
            ),
            Some(3),
        );
    }

    #[test]
    fn split_form_without_value_falls_through() {
        assert_eq!(resolve_from(argv(&["--test-threads"]), None), None);
    }

    #[test]
    fn both_unset_returns_none() {
        assert_eq!(resolve_from(argv(&[]), None), None);
    }

    #[test]
    fn public_wrapper_always_returns_positive() {
        assert!(resolve_test_threads() >= 1);
    }
}
