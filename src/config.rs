//! Resolved CLI / environment configuration for a test run.
//!
//! [`Config`] is built once per invocation by [`Config::parse`] (reading
//! `env::args()` and every environment variable at startup) and then passed
//! to every runtime constructor, every suite setup, and — via
//! [`crate::runtime::Runtime::config`] — to any downstream code that wants
//! to inspect the flags the test binary was launched with.

use std::collections::BTreeMap;
use std::env;
use std::io;
use std::io::IsTerminal as _;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::parallelism::{HardLimit, HardLimitGuard};

/// Compile-time cargo metadata captured from `env!(...)` at the user's
/// `#[rudzio::main]` expansion site. Lets test bodies resolve fixture
/// paths relative to the test crate's manifest without calling out to
/// `cargo` or parsing `Cargo.toml` at runtime.
///
/// Construct with the [`cargo_meta!`](crate::cargo_meta) macro — it
/// expands to the `env!(...)` block in the caller's crate:
///
/// ```rust,ignore
/// let meta = rudzio::cargo_meta!();
/// ```
#[derive(Debug, Clone)]
pub struct CargoMeta {
    /// `env!("CARGO_MANIFEST_DIR")` — absolute path to the crate that
    /// invoked `#[rudzio::main]`.
    pub manifest_dir: PathBuf,
    /// `env!("CARGO_PKG_NAME")`.
    pub pkg_name: String,
    /// `env!("CARGO_PKG_VERSION")`.
    pub pkg_version: String,
    /// `env!("CARGO_CRATE_NAME")` — the `pkg_name` with `-` replaced by
    /// `_`, or the target name for renamed targets.
    pub crate_name: String,
}

/// Output rendering style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One line per test with status and elapsed time.
    Pretty,
    /// One character per test (`.`/`F`/`c`/`i`) on a single line.
    Terse,
}

/// ANSI colour policy for terminal output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Enable colour if stdout is a TTY and `NO_COLOR` is unset.
    Auto,
    /// Force colour on.
    Always,
    /// Force colour off.
    Never,
}

/// How `#[ignore]`d tests should be treated for this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunIgnoredMode {
    /// Default: skip tests marked `#[ignore]`, report them as ignored.
    Normal,
    /// `--ignored`: only run ignored tests.
    Only,
    /// `--include-ignored`: run every test, ignored or not.
    Include,
}

/// Test-output rendering strategy.
///
/// `Live` drives a bottom-of-terminal live region with one row pair per
/// runtime slot (status + latest-output hint) plus an append-only history
/// region above it (see `crate::output::render`). `Plain` skips the live
/// region and prints linear append-only output suitable for log files and
/// CI pipelines.
///
/// Resolution rules (in order): explicit `--output=<mode>` or `--plain`
/// wins; otherwise `Live` iff stdout is a terminal AND the `CI`
/// environment variable is unset; otherwise `Plain`. See
/// [`OutputMode::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Bottom-of-terminal live region + history above.
    Live,
    /// Linear append-only output, one line per event.
    Plain,
}

impl OutputMode {
    /// Pick an [`OutputMode`] from an explicit user choice plus the
    /// ambient environment. `explicit` comes from `--output=` / `--plain`;
    /// `env` is the snapshot captured at startup (the `CI` key is used as
    /// a "definitely not a human terminal" hint even when stdout IS a
    /// TTY, because CI log capture frequently makes ANSI cursor-moves
    /// unreadable downstream).
    #[must_use]
    pub fn resolve(explicit: Option<Self>, env: &BTreeMap<String, String>) -> Self {
        if let Some(m) = explicit {
            return m;
        }
        if io::stdout().is_terminal() && !env.contains_key("CI") {
            Self::Live
        } else {
            Self::Plain
        }
    }
}

/// How `#[rudzio::test(benchmark = ...)]`-annotated tests should be run.
///
/// The annotation is deliberately additive: a bench-annotated test is a
/// regular test whose body the macro knows how to run repeatedly under a
/// [`crate::bench::Strategy`]. Whether the macro actually dispatches to the
/// strategy is decided at runtime from this mode, so the same binary can
/// serve both `cargo test` (smoke-mode iteration count = 1) and
/// `cargo test -- --bench` (full strategy execution).
///
/// [`BenchMode::Smoke`] is the default — `cargo test` on a bench-annotated
/// test runs its body once as a regular test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BenchMode {
    /// Default: run the body once as a regular test, ignore the
    /// `benchmark = ...` argument. Keeps `cargo test` fast on CI while
    /// still exercising the bench body for correctness.
    #[default]
    Smoke,
    /// `--bench`: dispatch each bench-annotated test through its
    /// strategy and render the resulting [`crate::bench::BenchReport`].
    /// Regular (non-benched) tests still run normally in this mode.
    Full,
    /// `--no-bench`: skip bench-annotated tests entirely (they're
    /// reported as ignored so the count still makes sense). Useful on
    /// slow CI where even the smoke run is too much.
    Skip,
}

/// Resolved configuration for a test run, aggregating every CLI flag the
/// runner understands plus the process environment. Handed to every runtime
/// constructor, every suite `setup`, and accessible from any running test
/// via [`crate::runtime::Runtime::config`] (and transitively from the suite
/// context through its runtime borrow).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct Config {
    /// Positional filter — runs only tests whose name contains this substring.
    pub filter: Option<String>,
    /// `--skip=<substring>` entries. A test is excluded if its name contains
    /// any of them.
    pub skip_filters: Vec<String>,
    /// OS worker-thread count the runtime should size its pool to. Resolved
    /// from `--test-threads`, `RUST_TEST_THREADS`, or
    /// [`thread::available_parallelism`] in that order.
    pub threads: usize,
    /// Maximum number of tests dispatched concurrently per runtime group.
    /// This is the *scheduler* knob (how many futures are in-flight at
    /// once); [`Self::threads`] is the *executor* knob (how many OS
    /// workers). When `--concurrency-limit` is not set, this defaults to
    /// [`Self::threads`] so single-flag invocations behave the same as
    /// libtest.
    pub concurrency_limit: usize,
    /// Hard cap on the total number of rudzio-dispatched test bodies actively
    /// polling at once, **across every runtime group**. This is rudzio's
    /// third — and outermost — concurrency knob and it **composes** with the
    /// other two:
    ///
    /// 1. The limit counts every thread that is actively polling a rudzio
    ///    test body, including the runner's own `(runtime, suite)`
    ///    group-dispatch threads and each runtime's internal worker-pool
    ///    threads (tokio multithread workers, compio reactors,
    ///    futures-executor pool, …). It is the **total** cap across all of
    ///    them, not per-runtime.
    /// 2. [`Self::threads`] sizes each runtime's internal worker pool;
    ///    [`Self::concurrency_limit`] caps in-flight futures per group.
    ///    `parallel_hardlimit` caps the product across the whole run — if
    ///    the first two would otherwise allow `groups × concurrency_limit`
    ///    test bodies to run simultaneously, this one holds that back to
    ///    the value here.
    /// 3. When the gate is hit, the polling OS thread **really parks** on
    ///    a `std::sync::Condvar` (not a cooperative async semaphore). This
    ///    is deliberate: it's the same backpressure mechanism the OS would
    ///    apply at the thread level and is what the user-facing name "hard
    ///    limit" is meant to convey.
    ///
    /// Resolution from `--threads-parallel-hardlimit=<value>`:
    ///
    /// | Form | Effective value |
    /// |---|---|
    /// | *(flag absent)* | `Some(threads)` (default gate) |
    /// | `=<N>` with N>0 | `Some(N)` |
    /// | `=threads` | `Some(threads)` (explicit spelling of default) |
    /// | `=none` | `None` (gate disabled) |
    /// | `=0` / garbage | falls back to default |
    ///
    /// When `--bench` is passed without an explicit hardlimit flag, the
    /// gate auto-disables (`None`) so benchmark timing isn't perturbed by
    /// `Condvar` wake-ups. An explicit `--threads-parallel-hardlimit=<N>`
    /// (or `=none`) wins over the auto-disable in either direction.
    ///
    /// # Caveat — current-thread runtimes
    ///
    /// On a single-thread runtime (`tokio::CurrentThread`,
    /// `futures::LocalPool`, …) the gate can deadlock if you set
    /// `parallel_hardlimit < concurrency_limit`: with N permits held, the
    /// sole thread parks on the Condvar trying to acquire the (N+1)th
    /// permit, and no other future on that thread can make progress to
    /// release one. The honest implementation exposes the mis-config
    /// rather than papering over it — set the hardlimit at least as high
    /// as the largest current-thread `concurrency_limit` in your run.
    pub parallel_hardlimit: Option<NonZeroUsize>,
    /// Shared [`HardLimit`] permit pool, constructed from
    /// [`Self::parallel_hardlimit`]. Held behind an [`Arc`] so every
    /// generated per-test fn (which only ever sees `&Config`) can acquire
    /// against the same gate without threading an extra parameter through
    /// the runner → owner → test-fn chain.
    ///
    /// `pub(crate)` rather than `pub` on purpose: the public knob is
    /// [`Self::parallel_hardlimit`]; the Arc is an implementation detail
    /// and may be replaced by a non-Arc construction if we later move to
    /// scoped threads for test dispatch.
    #[doc(hidden)]
    pub hardlimit: Arc<HardLimit>,
    /// Output format.
    pub format: Format,
    /// Colour policy.
    pub color: ColorMode,
    /// How `#[ignore]`d tests are treated.
    pub run_ignored: RunIgnoredMode,
    /// How `#[rudzio::test(benchmark = ...)]`-annotated tests are treated.
    pub bench_mode: BenchMode,
    /// Rendering strategy for the runner's test output.
    pub output_mode: OutputMode,
    /// `--list`: print test names and exit without running.
    pub list: bool,
    /// `--help` / `-h`: print a usage message listing every recognised
    /// flag and environment variable, then exit. Handled by the runner
    /// (see `crate::runner::run`) so the help text reaches the real
    /// terminal rather than the capture pipe.
    pub help: bool,
    /// `--test-timeout=<secs>`. `None` = unbounded.
    pub test_timeout: Option<Duration>,
    /// `--run-timeout=<secs>`. `None` = unbounded.
    pub run_timeout: Option<Duration>,
    /// `--suite-setup-timeout=<secs>`. Default budget for `Suite::setup`.
    /// `None` = unbounded.
    pub suite_setup_timeout: Option<Duration>,
    /// `--suite-teardown-timeout=<secs>`. Default budget for
    /// `Suite::teardown`. `None` = unbounded.
    pub suite_teardown_timeout: Option<Duration>,
    /// `--test-setup-timeout=<secs>`. Default budget for `Suite::context`
    /// (per-test setup). Overridden per-test by
    /// `#[rudzio::test(setup_timeout = ...)]`. `None` = unbounded.
    pub test_setup_timeout: Option<Duration>,
    /// `--test-teardown-timeout=<secs>`. Default budget for
    /// `Test::teardown`. Overridden per-test by
    /// `#[rudzio::test(teardown_timeout = ...)]`. `None` = unbounded.
    pub test_teardown_timeout: Option<Duration>,
    /// `--phase-hang-grace=<secs>`. Layer-2 grace window applied
    /// *after* a phase blows its budget: the wrapper cancels the
    /// phase token, then waits this long for the phase future to
    /// drop voluntarily before escalating to `Hung` and firing the
    /// runtime abort handle. `None` (encoded by `--phase-hang-grace=0`)
    /// disables the escalation — phases that exceed their budget
    /// stop at `TimedOut` and the binary relies on Layer 1 for any
    /// remaining cleanup.
    ///
    /// Defaults to `Some(2s)` — short enough that pathological hangs
    /// don't slow the tear-down significantly, long enough that
    /// real-world cooperative cancellation has time to land.
    pub phase_hang_grace: Option<Duration>,
    /// `--cancel-grace-period=<secs>`. Layer-1 process-exit
    /// safety net: after `root_token.cancel()` fires (SIGINT,
    /// SIGTERM, or `--run-timeout`), a watchdog thread sleeps this
    /// long, then `process::exit(2)` if the binary hasn't already
    /// terminated. Catches the case where a sync-blocked task can't
    /// be unblocked by any cooperative mechanism. `None` (encoded by
    /// `--cancel-grace-period=0`) disables the watchdog.
    ///
    /// Defaults to `Some(5s)` — comfortably above `--phase-hang-grace`'s
    /// 2s default so Layer 2 has a real chance to land before Layer 1
    /// fires, and below the 10s SIGKILL grace period most CI systems
    /// apply.
    pub cancel_grace_period: Option<Duration>,
    /// CLI arguments the runner did not recognise, preserved verbatim for
    /// downstream parsing by user code / custom runtimes.
    pub unparsed: Vec<String>,
    /// Snapshot of every environment variable at runner start. `BTreeMap`
    /// so iteration order is deterministic across runs.
    pub env: BTreeMap<String, String>,
    /// Compile-time cargo metadata from the crate whose `#[rudzio::main]`
    /// kicked off this run. Non-optional on purpose: `manifest_dir` is
    /// the kind of field where "maybe absent" is a trap. If you need a
    /// `Config` outside `#[rudzio::main]`, construct one with
    /// [`cargo_meta!`](crate::cargo_meta).
    pub cargo: CargoMeta,
}

impl Config {
    /// Read from `env::args()` and the whole process environment. Unknown
    /// flags are preserved in [`Self::unparsed`] instead of being dropped.
    /// `cargo` comes from the call site via [`cargo_meta!`](crate::cargo_meta)
    /// because the relevant values are only available as compile-time
    /// `env!(...)` expansions in the user's crate.
    #[must_use]
    pub fn parse(cargo: CargoMeta) -> Self {
        let argv: Vec<String> = env::args().skip(1).collect();
        let env_snapshot: BTreeMap<String, String> = env::vars().collect();
        Self::from_argv_and_env(argv, env_snapshot, cargo)
    }

    /// Test-friendly parser. Takes argv + env explicitly so unit tests can
    /// exercise parsing without touching the ambient process environment.
    #[must_use]
    pub fn from_argv_and_env(
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        cargo: CargoMeta,
    ) -> Self {
        #[derive(Clone, Copy, Default)]
        enum HardlimitArg {
            #[default]
            Unset,
            Disabled,
            Threads,
            Explicit(NonZeroUsize),
        }

        fn classify_hardlimit(s: &str) -> Option<HardlimitArg> {
            match s {
                "none" => Some(HardlimitArg::Disabled),
                "threads" => Some(HardlimitArg::Threads),
                _ => s
                    .parse::<usize>()
                    .ok()
                    .and_then(NonZeroUsize::new)
                    .map(HardlimitArg::Explicit),
            }
        }

        let mut filter: Option<String> = None;
        let mut skip_filters: Vec<String> = Vec::new();
        let mut threads: Option<usize> = None;
        let mut concurrency_limit: Option<usize> = None;
        let mut hardlimit_arg = HardlimitArg::Unset;
        let mut format = Format::Pretty;
        let mut color = ColorMode::Auto;
        let mut run_ignored = RunIgnoredMode::Normal;
        let mut bench_mode = BenchMode::Smoke;
        let mut output_mode_explicit: Option<OutputMode> = None;
        let mut list = false;
        let mut help = false;
        let mut test_timeout: Option<Duration> = None;
        let mut run_timeout: Option<Duration> = None;
        let mut suite_setup_timeout: Option<Duration> = None;
        let mut suite_teardown_timeout: Option<Duration> = None;
        let mut test_setup_timeout: Option<Duration> = None;
        let mut test_teardown_timeout: Option<Duration> = None;
        // Layer-2 grace defaults to OFF (`None`). When set, a phase
        // that exceeds its budget is given the grace window to drop
        // cooperatively before escalating to `Hung`. Off-by-default
        // because the grace step polls the cancellable past
        // `phase_token.cancel()`, which can let cooperative bodies
        // observe their cancellation and run cleanup before the
        // wrapper drops them — a behaviour change vs the
        // pre-Layer-2 "drop on cancel immediately" semantic that
        // existing fixtures rely on.
        let mut phase_hang_grace: Option<Duration> = None;
        // Layer-1 process-exit grace defaults to 5s; sync-blocked
        // tasks ignoring SIGINT have always-on protection from
        // `process::exit(2)` so the binary can't hang forever.
        let mut cancel_grace_period: Option<Duration> = Some(Duration::from_secs(5));
        let mut unparsed: Vec<String> = Vec::new();

        let mut i = 0_usize;
        while i < argv.len() {
            let arg = &argv[i];

            if let Some(rest) = arg.strip_prefix("--test-threads=") {
                if let Ok(n) = rest.parse::<usize>()
                    && n > 0
                {
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
            } else if let Some(rest) = arg.strip_prefix("--concurrency-limit=") {
                if let Ok(n) = rest.parse::<usize>()
                    && n > 0
                {
                    concurrency_limit = Some(n);
                }
            } else if arg == "--concurrency-limit" {
                i += 1;
                if let Some(next) = argv.get(i)
                    && let Ok(n) = next.parse::<usize>()
                    && n > 0
                {
                    concurrency_limit = Some(n);
                }
            } else if let Some(rest) = arg.strip_prefix("--threads-parallel-hardlimit=") {
                if let Some(h) = classify_hardlimit(rest) {
                    hardlimit_arg = h;
                }
            } else if arg == "--threads-parallel-hardlimit" {
                i += 1;
                if let Some(h) = argv.get(i).and_then(|next| classify_hardlimit(next)) {
                    hardlimit_arg = h;
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
                format = if rest == "terse" {
                    Format::Terse
                } else {
                    Format::Pretty
                };
            } else if arg == "--format" {
                i += 1;
                if argv.get(i).is_some_and(|s| s == "terse") {
                    format = Format::Terse;
                }
            } else if arg == "--ignored" {
                run_ignored = RunIgnoredMode::Only;
            } else if arg == "--include-ignored" {
                run_ignored = RunIgnoredMode::Include;
            } else if arg == "--bench" {
                bench_mode = BenchMode::Full;
            } else if arg == "--no-bench" {
                bench_mode = BenchMode::Skip;
            } else if let Some(rest) = arg.strip_prefix("--output=") {
                output_mode_explicit = match rest {
                    "live" => Some(OutputMode::Live),
                    "plain" => Some(OutputMode::Plain),
                    _ => output_mode_explicit,
                };
            } else if arg == "--output" {
                i += 1;
                if let Some(next) = argv.get(i) {
                    output_mode_explicit = match next.as_str() {
                        "live" => Some(OutputMode::Live),
                        "plain" => Some(OutputMode::Plain),
                        _ => output_mode_explicit,
                    };
                }
            } else if arg == "--plain" {
                output_mode_explicit = Some(OutputMode::Plain);
            } else if arg == "--list" {
                list = true;
            } else if arg == "--help" || arg == "-h" {
                help = true;
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
            } else if let Some(rest) = arg.strip_prefix("--suite-setup-timeout=") {
                if let Ok(secs) = rest.parse::<u64>() {
                    suite_setup_timeout = Some(Duration::from_secs(secs));
                }
            } else if arg == "--suite-setup-timeout" {
                i += 1;
                if let Some(next) = argv.get(i)
                    && let Ok(secs) = next.parse::<u64>()
                {
                    suite_setup_timeout = Some(Duration::from_secs(secs));
                }
            } else if let Some(rest) = arg.strip_prefix("--suite-teardown-timeout=") {
                if let Ok(secs) = rest.parse::<u64>() {
                    suite_teardown_timeout = Some(Duration::from_secs(secs));
                }
            } else if arg == "--suite-teardown-timeout" {
                i += 1;
                if let Some(next) = argv.get(i)
                    && let Ok(secs) = next.parse::<u64>()
                {
                    suite_teardown_timeout = Some(Duration::from_secs(secs));
                }
            } else if let Some(rest) = arg.strip_prefix("--test-setup-timeout=") {
                if let Ok(secs) = rest.parse::<u64>() {
                    test_setup_timeout = Some(Duration::from_secs(secs));
                }
            } else if arg == "--test-setup-timeout" {
                i += 1;
                if let Some(next) = argv.get(i)
                    && let Ok(secs) = next.parse::<u64>()
                {
                    test_setup_timeout = Some(Duration::from_secs(secs));
                }
            } else if let Some(rest) = arg.strip_prefix("--test-teardown-timeout=") {
                if let Ok(secs) = rest.parse::<u64>() {
                    test_teardown_timeout = Some(Duration::from_secs(secs));
                }
            } else if arg == "--test-teardown-timeout" {
                i += 1;
                if let Some(next) = argv.get(i)
                    && let Ok(secs) = next.parse::<u64>()
                {
                    test_teardown_timeout = Some(Duration::from_secs(secs));
                }
            } else if let Some(rest) = arg.strip_prefix("--phase-hang-grace=") {
                if let Ok(secs) = rest.parse::<u64>() {
                    phase_hang_grace = if secs == 0 {
                        None
                    } else {
                        Some(Duration::from_secs(secs))
                    };
                }
            } else if arg == "--phase-hang-grace" {
                i += 1;
                if let Some(next) = argv.get(i)
                    && let Ok(secs) = next.parse::<u64>()
                {
                    phase_hang_grace = if secs == 0 {
                        None
                    } else {
                        Some(Duration::from_secs(secs))
                    };
                }
            } else if let Some(rest) = arg.strip_prefix("--cancel-grace-period=") {
                if let Ok(secs) = rest.parse::<u64>() {
                    cancel_grace_period = if secs == 0 {
                        None
                    } else {
                        Some(Duration::from_secs(secs))
                    };
                }
            } else if arg == "--cancel-grace-period" {
                i += 1;
                if let Some(next) = argv.get(i)
                    && let Ok(secs) = next.parse::<u64>()
                {
                    cancel_grace_period = if secs == 0 {
                        None
                    } else {
                        Some(Duration::from_secs(secs))
                    };
                }
            } else if let Some(rest) = arg.strip_prefix("--skip=") {
                skip_filters.push(rest.to_owned());
            } else if arg == "--skip" {
                i += 1;
                if let Some(next) = argv.get(i) {
                    skip_filters.push(next.clone());
                }
            } else if !arg.starts_with('-') {
                filter = Some(arg.clone());
            } else {
                unparsed.push(arg.clone());
            }

            i += 1;
        }

        let threads = threads
            .or_else(|| {
                env.get("RUST_TEST_THREADS")
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|n| *n > 0)
            })
            .unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZeroUsize::get));

        // `concurrency_limit` defaults to `threads` so the single-flag
        // `--test-threads=N` invocation keeps behaving the way libtest users
        // expect: N worker threads, N tests in-flight.
        let concurrency_limit = concurrency_limit.unwrap_or(threads);

        // `threads` is guaranteed >= 1 by the resolution chain above
        // (available_parallelism returns NonZeroUsize); the fallback is
        // unreachable in practice but keeps us off unwrap/expect.
        let threads_nz = NonZeroUsize::new(threads).unwrap_or(NonZeroUsize::MIN);
        let parallel_hardlimit: Option<NonZeroUsize> = match hardlimit_arg {
            HardlimitArg::Unset => {
                if bench_mode == BenchMode::Full {
                    None
                } else {
                    Some(threads_nz)
                }
            }
            HardlimitArg::Disabled => None,
            HardlimitArg::Threads => Some(threads_nz),
            HardlimitArg::Explicit(n) => Some(n),
        };

        let output_mode = OutputMode::resolve(output_mode_explicit, &env);

        let hardlimit = Arc::new(HardLimit::new(parallel_hardlimit));

        Self {
            filter,
            skip_filters,
            threads,
            concurrency_limit,
            parallel_hardlimit,
            hardlimit,
            format,
            color,
            run_ignored,
            bench_mode,
            output_mode,
            list,
            help,
            test_timeout,
            run_timeout,
            suite_setup_timeout,
            suite_teardown_timeout,
            test_setup_timeout,
            test_teardown_timeout,
            phase_hang_grace,
            cancel_grace_period,
            unparsed,
            env,
            cargo,
        }
    }

    /// Acquire one permit from the process-wide parallel-execution gate.
    /// Intended for macro-generated per-test code — users shouldn't need
    /// to call this directly. Returns a no-op guard when the gate is
    /// disabled ([`Self::parallel_hardlimit`] is `None`).
    #[doc(hidden)]
    #[must_use]
    pub fn acquire_hardlimit_permit(&self) -> HardLimitGuard<'_> {
        self.hardlimit.acquire()
    }
}

/// Human-readable usage string emitted by `--help` / `-h`. Lives on
/// `Config` so the runner and any custom aggregator (hand-rolled
/// `#[rudzio::main]` call site) print the same canonical text.
pub const USAGE: &str = "\
rudzio test runner

USAGE:
    <test-binary> [OPTIONS] [FILTER]

ARGUMENTS:
    <FILTER>                    Positional substring; only tests whose name
                                contains this substring run. Combine with
                                --skip to carve out a complementary subset.

OPTIONS:
    --skip <SUBSTRING>          Exclude tests whose name contains <SUBSTRING>.
                                Repeatable — accumulates across occurrences.
    --ignored                   Only run tests marked #[ignore].
    --include-ignored           Run every test, ignored or not.
    --bench                     Dispatch #[rudzio::test(benchmark=...)] tests
                                through their strategy. Non-bench tests still
                                run.
    --no-bench                  Skip bench-annotated tests (reported as
                                ignored).
    --list                      Print test names (one per line, libtest
                                format) and exit without running anything.
    --test-threads <N>          OS worker-thread count runtimes size their
                                pool to. Defaults to RUST_TEST_THREADS, else
                                std::thread::available_parallelism().
    --concurrency-limit <N>     Max in-flight tests per runtime group.
                                Defaults to --test-threads.
    --format <pretty|terse>     Output format. Default: pretty.
    --color <auto|always|never> ANSI colour policy. Default: auto.
    --output <live|plain>       Output rendering. 'live' = bottom-of-terminal
                                live region + history above (default on TTY
                                with CI unset). 'plain' = linear append-only
                                (default off-TTY or under CI).
    --plain                     Shorthand for --output=plain.
    --test-timeout <SECS>       Per-test budget. On expiry, the per-test
                                cancellation token fires and teardown runs.
    --run-timeout <SECS>        Whole-run budget. Cancels the root token;
                                in-flight tests wind down, queued tests are
                                reported as cancelled, teardowns run.
    --phase-hang-grace <SECS>   Layer-2 grace after a phase exceeds its
                                budget. The phase token is cancelled and
                                the future is given this long to drop
                                voluntarily before the wrapper escalates
                                to [HANG] and fires the abort handle.
                                Default 2s. Set to 0 to disable.
    --cancel-grace-period <SECS>
                                Layer-1 process-exit safety net. After
                                root cancellation (SIGINT / SIGTERM /
                                --run-timeout), the watchdog sleeps this
                                long, then process::exit(2) if the binary
                                hasn't already terminated. Default 5s.
                                Set to 0 to disable.
    -h, --help                  Print this message and exit.

ENVIRONMENT:
    RUST_TEST_THREADS           Default for --test-threads when the flag
                                is absent.
    NO_COLOR                    If set (any value) and --color=auto, colour
                                off.
    FORCE_COLOR                 If set (any value), colour on regardless of
                                --color and TTY status.
    CI                          If set and --output is absent, selects
                                --output=plain even on a TTY (CI log
                                capture often can't render the live region).

EXIT STATUS:
    0   every test passed (or none ran).
    1   at least one test failed, panicked, was cancelled, or timed out;
        or a teardown failure fired.
    2   runner setup error (output capture init, etc.).

Unknown flags are preserved in Config::unparsed for downstream parsing
by custom runtimes or test helpers — they do not produce an error.
";
