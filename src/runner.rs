use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::env;
use std::io::{self, IsTerminal as _, Write as _};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt as _};
use tokio_util::sync::CancellationToken;

use crate::runtime::DynRuntime;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunIgnored {
    /// Default: skip ignored tests (report as ignored).
    Normal,
    /// `--ignored`: run only tests marked `#[ignore]`.
    Only,
    /// `--include-ignored`: run all tests.
    Include,
}

#[derive(Debug)]
struct CliArgs {
    filter: Option<String>,
    skip_filters: Vec<String>,
    threads: usize,
    format: Format,
    color: ColorMode,
    run_ignored: RunIgnored,
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
    let mut run_ignored = RunIgnored::Normal;
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
            run_ignored = RunIgnored::Only;
        } else if arg == "--include-ignored" {
            run_ignored = RunIgnored::Include;
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
// Internal outcome types
// ---------------------------------------------------------------------------

/// Result of executing the test future, produced inside `spawn_dyn`.
#[derive(Debug)]
enum SpawnResult {
    Completed(Result<(), crate::test_case::BoxError>),
    Panicked,
    TimedOut,
    /// The root cancellation token fired before the test finished.
    Cancelled,
}

/// Per-test outcome returned by `spawn_test`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum TestOutcome {
    Passed { elapsed: Duration },
    Failed { elapsed: Duration, message: String },
    Panicked { elapsed: Duration },
    TimedOut,
    Cancelled,
}

// ---------------------------------------------------------------------------
// resolve_test_threads  (public; kept for callers)
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
// run()
// ---------------------------------------------------------------------------

/// Collect all registered [`TestToken`]s, group them by runtime+global pair,
/// run each group in its own OS thread, print results in cargo-test format,
/// then exit the process.
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
                RunIgnored::Normal | RunIgnored::Include => true,
                RunIgnored::Only => t.ignored,
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
    // Global contexts receive a child of this token, so cancellation fans out
    // to every in-flight test via the framework's context plumbing.
    let root_token = CancellationToken::new();

    // Always install a SIGINT/SIGTERM handler on Unix so the runner cancels
    // gracefully instead of being killed. A dedicated thread iterates the
    // signal queue and cancels the root token on the first delivery.
    install_signal_handler(root_token.clone());

    // Global run timeout watchdog — cancels the root token and lets the run
    // wind down gracefully rather than aborting the process.
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

    let failures: Arc<Mutex<Vec<FailureInfo>>> = Arc::new(Mutex::new(Vec::new()));

    let mut groups: HashMap<TypeId, Vec<&'static TestToken>> = HashMap::new();
    for token in &filtered_tokens {
        groups.entry((token.runtime_group)()).or_default().push(token);
    }

    let start = Instant::now();

    let handles: Vec<_> = groups
        .into_values()
        .map(|group_tokens| {
            let failures = Arc::clone(&failures);
            let test_timeout = args.test_timeout;
            let threads = args.threads;
            let run_ignored = args.run_ignored;
            let fmt = args.format;
            let group_token = root_token.clone();
            thread::spawn(move || {
                run_group(
                    group_tokens, threads, test_timeout, run_ignored, colored, fmt, failures,
                    group_token,
                )
            })
        })
        .collect();

    let total = handles
        .into_iter()
        .fold(TestSummary::zero(), |acc, handle| match handle.join() {
            Ok(summary) => acc.merge(summary),
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

    // Terse: newline after dots.
    if args.format == Format::Terse && total_count > 0 {
        println!();
    }

    // Failures section.
    let guard = failures.lock().expect("failures mutex poisoned");
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
// Group execution
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_group(
    mut tokens: Vec<&'static TestToken>,
    threads: usize,
    test_timeout: Option<Duration>,
    run_ignored: RunIgnored,
    colored: bool,
    fmt: Format,
    failures: Arc<Mutex<Vec<FailureInfo>>>,
    root_token: CancellationToken,
) -> TestSummary {
    tokens.sort_by_key(|t| (t.file, t.line));
    let first = tokens[0];
    let runtime_name = first.runtime_name;

    let dyn_rt: &'static dyn DynRuntime = match (first.make_runtime)() {
        Ok(rt) => Box::leak(rt),
        Err(e) => {
            eprintln!("error: FATAL: failed to create runtime [{runtime_name}]: {e}");
            return TestSummary {
                panicked: tokens.len(),
                total: tokens.len(),
                ..TestSummary::zero()
            };
        }
    };

    let fut = group_future(
        dyn_rt, tokens, runtime_name, threads, test_timeout, run_ignored, colored, fmt, failures,
        root_token,
    );
    let erased: Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send + 'static>> =
        Box::pin(async move {
            let summary: Box<dyn Any + Send> = Box::new(fut.await);
            summary
        });
    let result = dyn_rt.block_on_erased(erased);
    *result
        .downcast::<TestSummary>()
        .unwrap_or_else(|_| unreachable!("block_on_erased produced unexpected type"))
}

#[allow(clippy::too_many_arguments)]
async fn group_future(
    dyn_rt: &'static dyn DynRuntime,
    tokens: Vec<&'static TestToken>,
    runtime_name: &'static str,
    threads: usize,
    test_timeout: Option<Duration>,
    run_ignored: RunIgnored,
    colored: bool,
    fmt: Format,
    failures: Arc<Mutex<Vec<FailureInfo>>>,
    root_token: CancellationToken,
) -> TestSummary {
    let first = tokens[0];

    let global_box: Box<dyn Any + Send + Sync> =
        match (first.make_global)(dyn_rt, root_token.clone()).await {
            Ok(g) => g,
            Err(e) => {
                eprintln!(
                    "error: FATAL: failed to create global context [{runtime_name}]: {e}"
                );
                return TestSummary {
                    panicked: tokens.len(),
                    total: tokens.len(),
                    ..TestSummary::zero()
                };
            }
        };

    let global_ptr: SendPtr<dyn Any + Send + Sync> = SendPtr(Box::into_raw(global_box));
    #[allow(unsafe_code)]
    let global_ref: &'static (dyn Any + Send + Sync) =
        unsafe { &*global_ptr.0 };

    let mut summary = TestSummary::zero();
    summary.total = tokens.len();

    let mut active: Vec<&'static TestToken> = Vec::new();
    for token in &tokens {
        let skip = match run_ignored {
            RunIgnored::Normal => token.ignored,
            RunIgnored::Only | RunIgnored::Include => false,
        };
        if skip {
            print_ignored_line(token, runtime_name, fmt, colored);
            summary.ignored += 1;
        } else {
            active.push(token);
        }
    }

    let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();
    let mut queued = active.into_iter();

    // Seed the in-flight pool only if we're not already cancelled. This
    // guarantees that a run cancelled before any test starts simply prints
    // each queued test as cancelled and returns.
    if !root_token.is_cancelled() {
        for _ in 0..threads {
            match queued.next() {
                Some(token) => {
                    in_flight.push(spawn_test(
                        dyn_rt,
                        global_ref,
                        token,
                        test_timeout,
                        root_token.clone(),
                    ));
                }
                None => break,
            }
        }
    }

    while let Some((token, outcome)) = in_flight.next().await {
        accumulate_outcome(
            token.name, runtime_name, &outcome, &mut summary, &failures, fmt, colored,
        );
        // Dispatch the next queued test only if the run has not been
        // cancelled. Already-running tests are allowed to finish gracefully;
        // not-yet-started ones are reported as cancelled after the loop.
        if !root_token.is_cancelled()
            && let Some(next) = queued.next()
        {
            in_flight.push(spawn_test(
                dyn_rt,
                global_ref,
                next,
                test_timeout,
                root_token.clone(),
            ));
        }
    }

    // Remaining queue entries were never dispatched because the run was
    // cancelled mid-stream (or never started it in the first place).
    for skipped in queued {
        print_cancelled_line(skipped, runtime_name, fmt, colored);
        summary.cancelled += 1;
    }

    #[allow(unsafe_code)]
    let global_box: Box<dyn Any + Send + Sync> =
        unsafe { Box::from_raw(global_ptr.0) };

    // Always run global teardown, even after cancellation, so per-group
    // resources get a chance to clean up. Catch unwind so a panicking
    // teardown cannot poison the runner thread.
    use futures_util::FutureExt as _;
    match std::panic::AssertUnwindSafe((first.teardown_global)(global_box))
        .catch_unwind()
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("warning: global teardown failed [{runtime_name}]: {e}");
        }
        Err(_payload) => {
            eprintln!("warning: global teardown panicked [{runtime_name}]");
        }
    }

    summary
}

fn accumulate_outcome(
    name: &'static str,
    runtime_name: &'static str,
    outcome: &TestOutcome,
    summary: &mut TestSummary,
    failures: &Arc<Mutex<Vec<FailureInfo>>>,
    fmt: Format,
    colored: bool,
) {
    print_test_result(name, runtime_name, outcome, fmt, colored);
    match outcome {
        TestOutcome::Passed { .. } => summary.passed += 1,
        TestOutcome::Failed { message, .. } => {
            summary.failed += 1;
            let mut guard = failures.lock().expect("failures mutex poisoned");
            guard.push(FailureInfo { name, message: message.clone() });
        }
        TestOutcome::Panicked { .. } => summary.panicked += 1,
        TestOutcome::TimedOut => summary.timed_out += 1,
        TestOutcome::Cancelled => summary.cancelled += 1,
    }
}

fn print_ignored_line(
    token: &'static TestToken,
    runtime_name: &'static str,
    fmt: Format,
    colored: bool,
) {
    match fmt {
        Format::Terse => {
            print!("{}", yellow("i", colored));
            let _flush = io::stdout().flush();
        }
        Format::Pretty => {
            let label = yellow("ignored", colored);
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

fn print_cancelled_line(
    token: &'static TestToken,
    runtime_name: &'static str,
    fmt: Format,
    colored: bool,
) {
    match fmt {
        Format::Terse => {
            print!("{}", yellow("c", colored));
            let _flush = io::stdout().flush();
        }
        Format::Pretty => {
            println!(
                "test {} [{}] ... {}",
                token.name,
                runtime_name,
                yellow("cancelled", colored),
            );
        }
    }
}

fn print_test_result(
    name: &'static str,
    runtime_name: &'static str,
    outcome: &TestOutcome,
    fmt: Format,
    colored: bool,
) {
    match fmt {
        Format::Terse => {
            let ch = match outcome {
                TestOutcome::Passed { .. } => ".".to_owned(),
                TestOutcome::Failed { .. } | TestOutcome::Panicked { .. } | TestOutcome::TimedOut => {
                    red("F", colored)
                }
                TestOutcome::Cancelled => yellow("c", colored),
            };
            print!("{ch}");
            let _flush = io::stdout().flush();
        }
        Format::Pretty => {
            let status = match outcome {
                TestOutcome::Passed { elapsed } => {
                    format!("{} ({elapsed:.2?})", green("ok", colored))
                }
                TestOutcome::Failed { elapsed, .. } => {
                    format!("{} ({elapsed:.2?})", red("FAILED", colored))
                }
                TestOutcome::Panicked { elapsed } => {
                    format!("{} ({elapsed:.2?})", red("FAILED (panicked)", colored))
                }
                TestOutcome::TimedOut => red("FAILED (timed out)", colored),
                TestOutcome::Cancelled => yellow("cancelled", colored),
            };
            println!("test {name} [{runtime_name}] ... {status}");
        }
    }
}

// ---------------------------------------------------------------------------
// spawn_test
// ---------------------------------------------------------------------------

async fn spawn_test(
    dyn_rt: &'static dyn DynRuntime,
    global: &'static (dyn Any + Send + Sync),
    token: &'static TestToken,
    test_timeout: Option<Duration>,
    root_token: CancellationToken,
) -> (&'static TestToken, TestOutcome) {
    let name = token.name;
    let start = Instant::now();

    // Per-test token: a child of the root token so root cancellation still
    // fans out, but isolated enough that a per-test timeout can cancel only
    // this test without affecting siblings.
    let per_test_token = root_token.child_token();

    let ctx_box: Box<dyn Any + Send> =
        match (token.make_test_ctx)(global, per_test_token.clone()).await {
            Ok(c) => c,
            Err(e) => {
                let elapsed = start.elapsed();
                let message = format!("failed to create test context: {e}");
                return (token, TestOutcome::Failed { elapsed, message });
            }
        };

    let ctx_ptr: SendPtr<dyn Any + Send> = SendPtr(Box::into_raw(ctx_box));
    #[allow(unsafe_code)]
    let ctx_ref: &'static mut (dyn Any + Send) = unsafe { &mut *ctx_ptr.0 };

    let test_fut = Box::pin(async move { (token.run)(ctx_ref).await });

    let spawn_result = dyn_rt
        .spawn_dyn(Box::pin(execute_test(
            test_fut,
            test_timeout,
            dyn_rt,
            per_test_token.clone(),
        )))
        .await;

    // SAFETY: spawn_dyn has completed; ctx_ref is no longer held by the future.
    #[allow(unsafe_code)]
    let ctx_box: Box<dyn Any + Send> = unsafe { Box::from_raw(ctx_ptr.0) };

    let elapsed = start.elapsed();

    let outcome = match spawn_result {
        Err(_join_err) => TestOutcome::Panicked { elapsed },
        Ok(boxed) => {
            let result = boxed
                .downcast::<SpawnResult>()
                .expect("spawn_test: unexpected result type from execute_test");
            match *result {
                SpawnResult::Panicked => TestOutcome::Panicked { elapsed },
                SpawnResult::TimedOut => TestOutcome::TimedOut,
                SpawnResult::Cancelled => TestOutcome::Cancelled,
                SpawnResult::Completed(Ok(())) => TestOutcome::Passed { elapsed },
                SpawnResult::Completed(Err(ref e)) => {
                    TestOutcome::Failed { elapsed, message: e.to_string() }
                }
            }
        }
    };

    // Always run per-test teardown — even on timeout, cancellation, or panic
    // — so user-side cleanup is guaranteed to run. The teardown itself is
    // also wrapped in `catch_unwind` so a panicking teardown cannot destroy
    // the runner's accounting.
    use futures_util::FutureExt as _;
    match std::panic::AssertUnwindSafe((token.teardown_test)(ctx_box))
        .catch_unwind()
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("warning: test teardown failed [{name}]: {e}");
        }
        Err(_payload) => {
            eprintln!("warning: test teardown panicked [{name}]");
        }
    }

    (token, outcome)
}

/// Runs the test future inside `spawn_dyn`, applying an optional per-test
/// timeout and honouring the run's root cancellation token.
async fn execute_test(
    test_fut: Pin<Box<dyn Future<Output = Result<(), crate::test_case::BoxError>> + Send>>,
    test_timeout: Option<Duration>,
    dyn_rt: &'static dyn DynRuntime,
    root_token: CancellationToken,
) -> Box<dyn Any + Send> {
    Box::new(run_with_timeout_and_cancel(test_fut, test_timeout, dyn_rt, root_token).await)
}

async fn run_with_timeout_and_cancel(
    test_fut: Pin<Box<dyn Future<Output = Result<(), crate::test_case::BoxError>> + Send>>,
    test_timeout: Option<Duration>,
    dyn_rt: &'static dyn DynRuntime,
    per_test_token: CancellationToken,
) -> SpawnResult {
    use futures_util::FutureExt as _;
    use futures_util::future::{Either, select};

    // Wrap the test future in `catch_unwind` so panics surface as
    // `SpawnResult::Panicked` instead of aborting the group thread, then
    // route it through `run_until_cancelled(per_test_token, …)` so any
    // cancellation — per-test timeout, root cancel, SIGINT/SIGTERM — collapses
    // the wrapped future on the next poll.
    let catch_fut = std::panic::AssertUnwindSafe(test_fut).catch_unwind();
    let cancellable = Box::pin(per_test_token.run_until_cancelled(catch_fut));

    if let Some(dur) = test_timeout {
        match select(cancellable, dyn_rt.sleep_dyn(dur)).await {
            // `run_until_cancelled` yields `Some(inner_output)` on completion
            // and `None` on cancellation.
            Either::Left((Some(Ok(r)), _)) => SpawnResult::Completed(r),
            Either::Left((Some(Err(_payload)), _)) => SpawnResult::Panicked,
            Either::Left((None, _)) => SpawnResult::Cancelled,
            Either::Right(_pending_test_fut) => {
                // The test did not finish before the per-test watchdog fired.
                // Cancel the per-test token so the test body observes the
                // signal and can wind down gracefully; dropping
                // `_pending_test_fut` at the end of this block then releases
                // its resources. We intentionally do not wait for graceful
                // shutdown here — the test has already blown its budget.
                per_test_token.cancel();
                SpawnResult::TimedOut
            }
        }
    } else {
        match cancellable.await {
            Some(Ok(r)) => SpawnResult::Completed(r),
            Some(Err(_payload)) => SpawnResult::Panicked,
            None => SpawnResult::Cancelled,
        }
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Install a best-effort SIGINT/SIGTERM handler on Unix that cancels `token`
/// on delivery, so in-flight tests can shut down cooperatively instead of
/// being killed.
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

/// Non-Unix fallback: no signal handling. The runner still exits normally
/// once its caller drops or when a test completes; platform-specific console
/// handlers could be added here later if needed.
#[cfg(not(unix))]
fn install_signal_handler(_token: CancellationToken) {}

// ---------------------------------------------------------------------------
// SendPtr
// ---------------------------------------------------------------------------

/// Raw-pointer newtype that asserts `Send`/`Sync` so the runner's async
/// group/test futures, which hand out a `&'static` view of a heap allocation
/// and reclaim it via `Box::from_raw`, remain `Send + Sync` across await points.
///
/// Safety: only used with owned `Box<T>` allocations where the target satisfies
/// the corresponding bounds (`T: Send + Sync` for the global context,
/// `T: Send` for per-test contexts). The pointer is owned, never shared, and
/// only dereferenced inside `#[allow(unsafe_code)]` blocks in this module.
#[derive(Debug)]
struct SendPtr<T: ?Sized>(*mut T);

#[allow(unsafe_code)]
// SAFETY: `SendPtr` owns its allocation exclusively; callers only wrap pointers
// whose targets are themselves `Send`.
unsafe impl<T: ?Sized + Send> Send for SendPtr<T> {}

#[allow(unsafe_code)]
// SAFETY: the pointer is dereferenced on a single thread at a time; callers
// only wrap pointers whose targets are themselves `Sync`.
unsafe impl<T: ?Sized + Sync> Sync for SendPtr<T> {}

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
