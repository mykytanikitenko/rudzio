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

/// Expand to a [`CargoMeta`] populated from the caller crate's
/// `env!(...)` values. Use this when you need to build a [`Config`]
/// outside `#[rudzio::main]` — for example in a unit test:
///
/// ```rust,ignore
/// let config = rudzio::Config::parse(rudzio::cargo_meta!());
/// ```
#[macro_export]
macro_rules! cargo_meta {
    () => {
        $crate::CargoMeta::new(
            env!("CARGO_CRATE_NAME").to_owned(),
            ::std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            env!("CARGO_PKG_NAME").to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        )
    };
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
                                Repeatable \u{2014} accumulates across occurrences.
    --exact                     Require the positional filter and every --skip
                                value to match the qualified test name exactly,
                                not as a substring. Mirrors libtest --exact.
    --ignored                   Only run tests marked #[ignore].
    --include-ignored           Run every test, ignored or not.
    --bench                     Dispatch #[rudzio::test(benchmark=...)] tests
                                through their strategy. Non-bench tests still
                                run.
    --no-bench                  Skip bench-annotated tests (reported as
                                ignored).
    --list                      Print test names (one per line, libtest
                                format) and exit without running anything.
    --logfile <FILE>            Append a libtest-shape per-test log
                                (`<status> <qualified_name>` per line,
                                statuses: ok / failed / ignored) to FILE.
                                File is truncated on open. Failures to
                                open the file are warned to stderr but
                                don't abort the run \u{2014} the on-screen
                                report still prints. Mirrors libtest
                                --logfile.
    --shuffle                   Permute each (runtime, suite) group's
                                test list before dispatch. Cross-group
                                ordering is unaffected (each group runs
                                on its own thread + runtime). The
                                resolved seed is printed as
                                `shuffle seed: <N>` so a re-run with
                                --shuffle-seed=<N> reproduces the order.
    --shuffle-seed <N>          Implies --shuffle and pins the seed to
                                <N> (a u64). Mirrors libtest
                                --shuffle-seed.
    --report-time               Accepted for libtest compatibility.
                                Per-test timing already prints by default
                                in pretty/plain output, so this flag is a
                                silent no-op rather than a duplicate.
    --ensure-time [<MS>]        Accepted for libtest compatibility.
                                Silently consumed; rudzio's own
                                --test-timeout/--run-timeout govern
                                cancellation. Use those for hard caps.
    --test-threads <N>          OS worker-thread count runtimes size their
                                pool to. Defaults to RUST_TEST_THREADS, else
                                std::thread::available_parallelism(). The
                                executor knob; pairs with --concurrency-limit
                                (scheduler) and --threads-parallel-hardlimit
                                (process-wide gate).
    --concurrency-limit <N>     Max in-flight tests per runtime group
                                (scheduler knob; --test-threads sizes the
                                executor underneath). Defaults to
                                --test-threads, so single-flag invocations
                                match libtest semantics: N workers, N tests
                                in-flight per group.
    --threads-parallel-hardlimit=<VALUE>
                                Process-wide cap on test bodies actively
                                polling at once, across every runtime group.
                                Composes with --test-threads and
                                --concurrency-limit (caps the product across
                                groups). When the gate is full, callers
                                yield cooperatively through a runtime-
                                agnostic async semaphore \u{2014} no thread parks,
                                so timer/IO/spawned-subtask wakers held by
                                permit-holders stay pollable. Accepted
                                values:
                                  <N>     pin the gate at N permits.
                                  threads pin at the resolved --test-threads
                                          (the default when the flag is
                                          absent and --bench is not set).
                                  none    disable the gate entirely.
                                Under --bench (without an explicit value
                                here) the gate auto-disables so benchmark
                                timing isn't perturbed by gate-induced
                                yields. An explicit value passed here wins
                                over the bench-mode default in either
                                direction.
    --format <pretty|terse>     Output format. Default: pretty.
    -q, --quiet                 Synonym for --format=terse (libtest compat).
    --color <auto|always|never> ANSI colour policy. Default: auto.
    --output <live|plain>       Output rendering. 'live' = bottom-of-terminal
                                live region + history above (default on TTY
                                with CI unset). 'plain' = linear append-only
                                (default off-TTY or under CI).
    --plain                     Shorthand for --output=plain.
    --test-timeout <SECS>       Per-test budget for the test body. On
                                expiry, the per-test cancellation token
                                fires and teardown runs. Unbounded when
                                absent. Override per test with
                                #[rudzio::test(timeout = ...)].
    --run-timeout <SECS>        Whole-run budget. Cancels the root token;
                                in-flight tests wind down, queued tests are
                                reported as cancelled, teardowns run.
                                Unbounded when absent.
    --suite-setup-timeout <SECS>
                                Default budget for Suite::setup. Unbounded
                                when absent.
    --suite-teardown-timeout <SECS>
                                Default budget for Suite::teardown.
                                Unbounded when absent.
    --test-setup-timeout <SECS>
                                Default budget for per-test setup
                                (Suite::context). Unbounded when absent.
                                Override per test with
                                #[rudzio::test(setup_timeout = ...)].
    --test-teardown-timeout <SECS>
                                Default budget for per-test teardown
                                (Test::teardown). Unbounded when absent.
                                Override per test with
                                #[rudzio::test(teardown_timeout = ...)].
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
    RUST_BACKTRACE              Standard libstd backtrace toggle. The runner
                                sets it to 'full' if unset on entry, so panic
                                messages always carry an actionable
                                backtrace. Export it explicitly to override.
    RUST_LIB_BACKTRACE          Same shape as RUST_BACKTRACE but for
                                library-level panics (anyhow, etc.). Also
                                defaulted to 'full' by the runner.

EXAMPLES:
    Pipe-safe output for log capture, log-shippers, or AI agents
    parsing the run (no ANSI, no live region, one event per line):

        <test-binary> --output=plain --color=never

    Run one specific test (positional = substring against the
    fully-qualified test name; no glob, no regex):

        <test-binary> my_failing_test

    Enumerate every test the binary knows about without running any.
    Output is libtest format (`<name>: test`), one per line:

        <test-binary> --list

    Fully serialise the run \u{2014} one test in flight, end to end. Most
    deterministic option for reproducing flakes and the easiest mode
    for an AI agent to reason over because tests run in registry order
    with no interleave:

        <test-binary> --test-threads=1 --concurrency-limit=1 \\
            --threads-parallel-hardlimit=1

    Disable the process-wide hardlimit when debugging a suspected gate
    deadlock (rest of the concurrency stack is untouched):

        <test-binary> --threads-parallel-hardlimit=none

    Bounded iteration budget for an AI agent. `--run-timeout` caps
    the whole invocation; `--test-timeout` caps any single test;
    `--phase-hang-grace=0` short-circuits the Layer-2 escalation so a
    misbehaving cancellation path can't extend the budget. Combined
    with pipe-safe output, the agent never blocks for longer than
    --run-timeout regardless of what the suite does:

        <test-binary> --run-timeout=120 --test-timeout=10 \\
            --phase-hang-grace=0 --output=plain --color=never

EXIT STATUS:
    0   every test passed (or none ran).
    1   at least one test failed, panicked, was cancelled, or timed out;
        or a teardown failure fired.
    2   runner setup error (output capture init, etc.).

Unknown flags and extra positional arguments are preserved in
Config::unparsed for downstream parsing by custom runtimes or test
helpers. The rudzio runner prints a one-line stderr notice listing them
on startup so that typos of known flags don't silently no-op, but they
do not abort the run.
";

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
#[non_exhaustive]
pub enum BenchMode {
    /// `--bench`: dispatch each bench-annotated test through its
    /// strategy and render the resulting [`crate::bench::Report`].
    /// Regular (non-benched) tests still run normally in this mode.
    Full,
    /// `--no-bench`: skip bench-annotated tests entirely (they're
    /// reported as ignored so the count still makes sense). Useful on
    /// slow CI where even the smoke run is too much.
    Skip,
    /// Default: run the body once as a regular test, ignore the
    /// `benchmark = ...` argument. Keeps `cargo test` fast on CI while
    /// still exercising the bench body for correctness.
    #[default]
    Smoke,
}

/// Compile-time cargo metadata captured at the macro expansion site.
///
/// Captured from `env!(...)` at the user's `#[rudzio::main]` expansion
/// site. Lets test bodies resolve fixture paths relative to the test
/// crate's manifest without calling out to `cargo` or parsing
/// `Cargo.toml` at runtime.
///
/// Construct with the [`cargo_meta!`](crate::cargo_meta) macro — it
/// expands to the `env!(...)` block in the caller's crate:
///
/// ```rust,ignore
/// let meta = rudzio::cargo_meta!();
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CargoMeta {
    /// `env!("CARGO_CRATE_NAME")` — the `pkg_name` with `-` replaced by
    /// `_`, or the target name for renamed targets.
    pub crate_name: String,
    /// `env!("CARGO_MANIFEST_DIR")` — absolute path to the crate that
    /// invoked `#[rudzio::main]`.
    pub manifest_dir: PathBuf,
    /// `env!("CARGO_PKG_NAME")`.
    pub pkg_name: String,
    /// `env!("CARGO_PKG_VERSION")`.
    pub pkg_version: String,
}

impl CargoMeta {
    /// Construct a `CargoMeta` from its component env-var values.
    /// Macro-generated code calls this via [`cargo_meta!`](crate::cargo_meta).
    #[inline]
    #[must_use]
    pub const fn new(
        crate_name: String,
        manifest_dir: PathBuf,
        pkg_name: String,
        pkg_version: String,
    ) -> Self {
        Self {
            crate_name,
            manifest_dir,
            pkg_name,
            pkg_version,
        }
    }
}

/// ANSI colour policy for terminal output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorMode {
    /// Force colour on.
    Always,
    /// Enable colour if stdout is a TTY and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Force colour off.
    Never,
}

/// Resolved configuration for a test run.
///
/// Aggregates every CLI flag the runner understands plus the process
/// environment. Handed to every runtime constructor, every suite
/// `setup`, and accessible from any running test via
/// [`crate::runtime::Runtime::config`] (and transitively from the
/// suite context through its runtime borrow).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct Config {
    /// How `#[rudzio::test(benchmark = ...)]`-annotated tests are treated.
    pub bench_mode: BenchMode,
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
    /// Compile-time cargo metadata from the crate whose `#[rudzio::main]`
    /// kicked off this run. Non-optional on purpose: `manifest_dir` is
    /// the kind of field where "maybe absent" is a trap. If you need a
    /// `Config` outside `#[rudzio::main]`, construct one with
    /// [`cargo_meta!`](crate::cargo_meta).
    pub cargo: CargoMeta,
    /// Colour policy.
    pub color: ColorMode,
    /// Cargo-test-compat flags that rudzio silently accepts because the
    /// requested behaviour is already a rudzio default (e.g.
    /// `--report-time` — per-test elapsed already prints unconditionally).
    /// Recorded in original input order so debug tooling, integration
    /// tests, and a future verbose mode can show "we saw this flag and
    /// intentionally did nothing with it" without spamming stderr on
    /// every run.
    pub compat_consumed: Vec<String>,
    /// Maximum number of tests dispatched concurrently per runtime group.
    /// This is the *scheduler* knob (how many futures are in-flight at
    /// once); [`Self::threads`] is the *executor* knob (how many OS
    /// workers). When `--concurrency-limit` is not set, this defaults to
    /// [`Self::threads`] so single-flag invocations behave the same as
    /// libtest.
    pub concurrency_limit: usize,
    /// Snapshot of every environment variable at runner start. `BTreeMap`
    /// so iteration order is deterministic across runs.
    pub env: BTreeMap<String, String>,
    /// `--ensure-time` (libtest compat). `Some` when the user passed
    /// the flag, carrying the resolved warn/critical thresholds. Tests
    /// that exceed `critical` are treated as failures (counted in
    /// [`crate::TestSummary::ensure_time_exceeded`] and propagated to
    /// the exit code). Resolution order:
    /// `--ensure-time=<warn-ms>,<critical-ms>` literal value > the
    /// `RUST_TEST_TIME_INTEGRATION` env var (same `<warn>,<critical>`
    /// shape) > libtest's integration-test defaults of 500ms / 1000ms.
    pub ensure_time: Option<EnsureTimeConfig>,
    /// `--exact` (libtest compat): when `true`, the positional [`Self::filter`]
    /// and every [`Self::skip_filters`] entry are interpreted as exact
    /// equality matches against the test's qualified name rather than
    /// substring matches. Defaults to `false` (substring matching, matching
    /// rudzio's pre-`--exact` behaviour and standard cargo-test invocations).
    pub exact_match: bool,
    /// Positional filter — runs only tests whose name contains this substring
    /// (or equals it exactly when [`Self::exact_match`] is `true`).
    pub filter: Option<String>,
    /// Output format.
    pub format: Format,
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
    /// `--help` / `-h`: print a usage message listing every recognised
    /// flag and environment variable, then exit. Handled by the runner
    /// (see `crate::runner::run`) so the help text reaches the real
    /// terminal rather than the capture pipe.
    pub help: bool,
    /// `--list`: print test names and exit without running.
    pub list: bool,
    /// `--logfile <PATH>` / `--logfile=<PATH>` (libtest compat). When
    /// present, the runner appends one libtest-format line per finished
    /// test to this path: `<status> <qualified_name>` (e.g. `ok foo::bar`,
    /// `failed foo::baz`). Absent when the flag is not supplied.
    pub logfile: Option<PathBuf>,
    /// Rendering strategy for the runner's test output.
    pub output_mode: OutputMode,
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
    /// 3. When the gate is hit, the calling test's future **yields
    ///    cooperatively** through a runtime-agnostic async semaphore
    ///    ([`futures_intrusive::sync::Semaphore`]). The OS thread is
    ///    free to poll other ready tasks while the test waits — including
    ///    any timers, IO, or spawned subtasks the permit-holders are
    ///    awaiting. Works identically under tokio (multi-thread /
    ///    current-thread / local), compio, embassy, and futures-executor.
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
    /// gate auto-disables (`None`) so benchmark timing isn't perturbed
    /// by gate-induced yields. An explicit `--threads-parallel-hardlimit=<N>`
    /// (or `=none`) wins over the auto-disable in either direction.
    pub parallel_hardlimit: Option<NonZeroUsize>,
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
    /// How `#[ignore]`d tests are treated.
    pub run_ignored: RunIgnoredMode,
    /// `--run-timeout=<secs>`. `None` = unbounded.
    pub run_timeout: Option<Duration>,
    /// `--shuffle` (libtest compat). When `true`, the runner permutes
    /// each `(runtime, suite)` group's test list before dispatch using
    /// [`Self::shuffle_seed`] (or a derived seed when the seed flag was
    /// not supplied). Implicitly enabled by `--shuffle-seed=<N>`.
    pub shuffle: bool,
    /// `--shuffle-seed=<N>` (libtest compat). When `Some`, that exact
    /// seed is used for the shuffle permutation; same seed → same
    /// order across runs. When `None` and [`Self::shuffle`] is `true`,
    /// the runner derives a seed from the wall clock at run start and
    /// prints it on a single `shuffle seed: <N>` stdout line so the
    /// user can reproduce the order.
    pub shuffle_seed: Option<u64>,
    /// `--skip=<substring>` entries. A test is excluded if its name contains
    /// any of them.
    pub skip_filters: Vec<String>,
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
    /// `--test-timeout=<secs>`. `None` = unbounded.
    pub test_timeout: Option<Duration>,
    /// OS worker-thread count the runtime should size its pool to. Resolved
    /// from `--test-threads`, `RUST_TEST_THREADS`, or
    /// [`thread::available_parallelism`] in that order.
    pub threads: usize,
    /// CLI arguments the runner did not recognise, preserved verbatim for
    /// downstream parsing by user code / custom runtimes.
    pub unparsed: Vec<String>,
}

/// Resolved warn/critical thresholds for `--ensure-time`.
///
/// Mirrors libtest's tiered timing gate: a test that runs longer than
/// `warn` is logged but not failed; one that exceeds `critical` is
/// counted as a failure and bumps the run's exit code. Both come from
/// `--ensure-time=<warn-ms>,<critical-ms>` if explicit, otherwise from
/// `RUST_TEST_TIME_INTEGRATION` (libtest's integration-test slot, the
/// closest analogue to a rudzio aggregator binary), otherwise libtest's
/// 500/1000 ms integration defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct EnsureTimeConfig {
    /// Soft threshold — exceeded tests get a stderr warning but stay
    /// passing.
    pub warn: Duration,
    /// Hard threshold — exceeded tests are reported as failures and
    /// flip the run's exit code.
    pub critical: Duration,
}

impl EnsureTimeConfig {
    /// Libtest's integration-test defaults: 500ms warn, 1000ms critical.
    /// Used when `--ensure-time` is bare and no env override is set.
    #[inline]
    #[must_use]
    pub const fn integration_defaults() -> Self {
        Self {
            warn: Duration::from_millis(500),
            critical: Duration::from_millis(1000),
        }
    }

    /// Classify `elapsed` against the configured thresholds.
    ///
    /// `None` → under `warn` (no violation).
    /// `Some(Warn)` → reached `warn` but under `critical` (advisory).
    /// `Some(Critical)` → reached `critical` (counts as a failure).
    #[inline]
    #[must_use]
    pub const fn violation(&self, elapsed: Duration) -> Option<EnsureTimeViolation> {
        if elapsed.as_nanos() >= self.critical.as_nanos() {
            Some(EnsureTimeViolation::Critical)
        } else if elapsed.as_nanos() >= self.warn.as_nanos() {
            Some(EnsureTimeViolation::Warn)
        } else {
            None
        }
    }
}

/// Classification of a test's elapsed time against an
/// [`EnsureTimeConfig`]'s thresholds. `Warn` is advisory (the test still
/// passes); `Critical` flips the run's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnsureTimeViolation {
    /// Elapsed reached the `critical` threshold; counts as a failure.
    Critical,
    /// Elapsed reached the `warn` threshold but stayed under `critical`;
    /// surfaced on stderr without flipping the exit code.
    Warn,
}

/// Output rendering style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// One line per test with status and elapsed time.
    #[default]
    Pretty,
    /// One character per test (`.`/`F`/`c`/`i`) on a single line.
    Terse,
}

/// Parser-stage state for `--ensure-time`.
///
/// `Default` defers warn/critical resolution to env + libtest defaults;
/// `Explicit(warn, critical)` captures the inline `=<warn-ms>,<critical-ms>`
/// values so the env var is bypassed. Kept separate from
/// [`EnsureTimeConfig`] so the parser doesn't need to read the env
/// snapshot — that's a `Config::from_argv_and_env` responsibility.
#[derive(Debug, Clone, Copy)]
enum EnsureTimeArg {
    /// Bare `--ensure-time` (or unparsable inline value). Resolution
    /// continues with `RUST_TEST_TIME_INTEGRATION` then defaults.
    Default,
    /// `--ensure-time=<warn-ms>,<critical-ms>` parsed cleanly.
    Explicit { warn: Duration, critical: Duration },
}

/// Resolution outcome for `--threads-parallel-hardlimit=<value>`.
///
/// `Default` is [`Self::Unset`], meaning "no flag observed, fall back to the
/// per-bench-mode default at resolution time".
#[derive(Clone, Copy, Default)]
enum HardlimitArg {
    /// `--threads-parallel-hardlimit=none`: gate is disabled outright.
    Disabled,
    /// `--threads-parallel-hardlimit=<N>` with N>0: pin the gate at exactly N.
    Explicit(NonZeroUsize),
    /// `--threads-parallel-hardlimit=threads`: pin the gate at the resolved
    /// thread count (explicit spelling of the default).
    Threads,
    /// Flag not observed; resolve to the per-bench-mode default.
    #[default]
    Unset,
}

/// Resolution outcome for a `--flag=<secs>` flag where `0` means disabled.
///
/// Distinguishes the three cases the parser cares about:
/// "wasn't this flag at all", "was this flag, value 0 = disabled",
/// and "was this flag, here's the duration".
enum OptionalDurationParse {
    /// Flag matched with explicit `0`, meaning the feature is disabled.
    Disabled,
    /// Flag did not match; caller should try the next handler.
    NotMatched,
    /// Flag matched with a non-zero duration.
    Set(Duration),
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
#[non_exhaustive]
pub enum OutputMode {
    /// Bottom-of-terminal live region + history above.
    Live,
    /// Linear append-only output, one line per event.
    Plain,
}

/// Mutable bag carrying the partial parsing state for a single argv pass.
///
/// Splitting the long `from_argv_and_env` flag-loop into `parse_argv` plus
/// per-flag-family helpers keeps cognitive complexity inside the parser
/// loop bounded — each helper handles one category of flag.
#[derive(Default)]
struct ParsedArgs {
    /// Resolved bench mode from `--bench` / `--no-bench`.
    bench_mode: BenchMode,
    /// Layer-1 process-exit watchdog grace from
    /// `--cancel-grace-period=<secs>`. `Some(Duration::ZERO)` is preserved
    /// only as a sentinel that gets remapped to `None` (disabled) at the
    /// final `Config` resolution.
    cancel_grace_period: Option<Duration>,
    /// Colour policy from `--color=<auto|always|never>`.
    color: ColorMode,
    /// Silent-consume audit trail for cargo-test-compat flags whose
    /// requested behaviour is already a rudzio default (e.g.
    /// `--report-time`). Each match pushes the flag's verbatim spelling
    /// here in original argv order.
    compat_consumed: Vec<String>,
    /// In-flight test cap from `--concurrency-limit=<N>`. `None` means the
    /// flag was absent — `Config` defaults this to `threads` at resolution.
    concurrency_limit: Option<usize>,
    /// `--ensure-time` (libtest compat). `Some(EnsureTimeArg::Default)`
    /// when the user passed the bare flag (defer env+default resolution
    /// to `Config::from_argv_and_env`); `Some(EnsureTimeArg::Explicit)`
    /// when an inline `--ensure-time=<warn,critical>` value was parsed
    /// (env is ignored). Garbage values fall back to `Default` so the
    /// flag still has effect (matches libtest's lenient parse).
    ensure_time: Option<EnsureTimeArg>,
    /// `--exact` (libtest compat): switches `filter` and `skip_filters`
    /// from substring matching to exact-equality matching.
    exact_match: bool,
    /// Positional substring filter (the first non-flag argument that survives
    /// every flag handler).
    filter: Option<String>,
    /// Output format from `--format=pretty|terse`.
    format: Format,
    /// Result of `--threads-parallel-hardlimit=<value>` parsing; combined
    /// with `bench_mode` and `threads` later to produce the final hardlimit.
    hardlimit_arg: HardlimitArg,
    /// `true` if `--help`/`-h` was seen.
    help: bool,
    /// `true` if `--list` was seen.
    list: bool,
    /// `--logfile=<PATH>` / `--logfile <PATH>`. `None` when the flag was
    /// absent.
    logfile: Option<PathBuf>,
    /// Explicit `--output=<live|plain>` / `--plain` choice. `None` means
    /// fall through to the auto-detection rule in [`OutputMode::resolve`].
    output_mode_explicit: Option<OutputMode>,
    /// Layer-2 phase-hang grace from `--phase-hang-grace=<secs>`. `None`
    /// means disabled (explicit `=0`); inherits `Config`'s default when the
    /// flag is absent.
    phase_hang_grace: Option<Duration>,
    /// `--include-ignored` / `--ignored` selection for ignored tests.
    run_ignored: RunIgnoredMode,
    /// Whole-run wall-clock cap from `--run-timeout=<secs>`.
    run_timeout: Option<Duration>,
    /// `true` if `--shuffle` was seen, or if `--shuffle-seed=<N>` was
    /// seen with a parseable value (libtest implies shuffle).
    shuffle: bool,
    /// `Some(N)` from `--shuffle-seed=<N>` / `--shuffle-seed <N>`. A
    /// garbage value falls through (leaves `None`) without enabling
    /// shuffle; this matches how libtest treats unparsable seeds.
    shuffle_seed: Option<u64>,
    /// Substring filters from each `--skip=<text>` flag, in order.
    skip_filters: Vec<String>,
    /// Per-suite setup phase budget from `--suite-setup-timeout=<secs>`.
    suite_setup_timeout: Option<Duration>,
    /// Per-suite teardown phase budget from `--suite-teardown-timeout=<secs>`.
    suite_teardown_timeout: Option<Duration>,
    /// Per-test setup phase budget from `--test-setup-timeout=<secs>`.
    test_setup_timeout: Option<Duration>,
    /// Per-test teardown phase budget from `--test-teardown-timeout=<secs>`.
    test_teardown_timeout: Option<Duration>,
    /// Per-test body budget from `--test-timeout=<secs>`.
    test_timeout: Option<Duration>,
    /// Worker-pool size request from `--test-threads=<N>`. `None` lets
    /// `RUST_TEST_THREADS` / `available_parallelism()` decide later.
    threads: Option<usize>,
    /// CLI args the rudzio parser didn't recognise — preserved verbatim
    /// for downstream parsing by user code / custom runtimes.
    unparsed: Vec<String>,
}

/// How `#[ignore]`d tests should be treated for this run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunIgnoredMode {
    /// `--include-ignored`: run every test, ignored or not.
    Include,
    /// Default: skip tests marked `#[ignore]`, report them as ignored.
    #[default]
    Normal,
    /// `--ignored`: only run ignored tests.
    Only,
}

impl Config {
    /// Acquire one permit from the process-wide parallel-execution gate.
    /// Intended for macro-generated per-test code — users shouldn't need
    /// to call this directly. Yields cooperatively (no OS-thread
    /// parking) when the gate is full; returns immediately with a
    /// no-op guard when the gate is disabled ([`Self::parallel_hardlimit`]
    /// is `None`).
    #[doc(hidden)]
    #[inline]
    pub async fn acquire_hardlimit_permit(&self) -> HardLimitGuard<'_> {
        self.hardlimit.acquire().await
    }

    /// Test-friendly parser. Takes argv + env explicitly so unit tests can
    /// exercise parsing without touching the ambient process environment.
    #[must_use]
    #[inline]
    pub fn from_argv_and_env(
        argv: &[String],
        env: BTreeMap<String, String>,
        cargo: CargoMeta,
    ) -> Self {
        let parsed = parse_argv(argv);

        let resolved_threads = parsed
            .threads
            .or_else(|| {
                env.get("RUST_TEST_THREADS")
                    .and_then(|val| val.parse::<usize>().ok())
                    .filter(|count| *count > 0)
            })
            .unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZeroUsize::get));

        // `concurrency_limit` defaults to `threads` so the single-flag
        // `--test-threads=N` invocation keeps behaving the way libtest users
        // expect: N worker threads, N tests in-flight.
        let resolved_concurrency_limit = parsed.concurrency_limit.unwrap_or(resolved_threads);

        // `threads` is guaranteed >= 1 by the resolution chain above
        // (available_parallelism returns NonZeroUsize); the fallback is
        // unreachable in practice but keeps us off unwrap/expect.
        let threads_nz = NonZeroUsize::new(resolved_threads).unwrap_or(NonZeroUsize::MIN);
        let parallel_hardlimit =
            resolve_parallel_hardlimit(parsed.hardlimit_arg, parsed.bench_mode, threads_nz);

        let output_mode = OutputMode::resolve(parsed.output_mode_explicit, &env);

        let hardlimit = Arc::new(HardLimit::new(parallel_hardlimit));

        Self {
            bench_mode: parsed.bench_mode,
            cancel_grace_period: parsed.cancel_grace_period,
            cargo,
            color: parsed.color,
            compat_consumed: parsed.compat_consumed,
            concurrency_limit: resolved_concurrency_limit,
            ensure_time: resolve_ensure_time(parsed.ensure_time, &env),
            env,
            exact_match: parsed.exact_match,
            filter: parsed.filter,
            format: parsed.format,
            hardlimit,
            help: parsed.help,
            list: parsed.list,
            logfile: parsed.logfile,
            output_mode,
            parallel_hardlimit,
            phase_hang_grace: parsed.phase_hang_grace,
            run_ignored: parsed.run_ignored,
            run_timeout: parsed.run_timeout,
            shuffle: parsed.shuffle,
            shuffle_seed: parsed.shuffle_seed,
            skip_filters: parsed.skip_filters,
            suite_setup_timeout: parsed.suite_setup_timeout,
            suite_teardown_timeout: parsed.suite_teardown_timeout,
            test_setup_timeout: parsed.test_setup_timeout,
            test_teardown_timeout: parsed.test_teardown_timeout,
            test_timeout: parsed.test_timeout,
            threads: resolved_threads,
            unparsed: parsed.unparsed,
        }
    }

    /// Read from `env::args()` and the whole process environment. Unknown
    /// flags are preserved in [`Self::unparsed`] instead of being dropped.
    /// `cargo` comes from the call site via [`cargo_meta!`](crate::cargo_meta)
    /// because the relevant values are only available as compile-time
    /// `env!(...)` expansions in the user's crate.
    #[must_use]
    #[inline]
    pub fn parse(cargo: CargoMeta) -> Self {
        let argv: Vec<String> = env::args().skip(1).collect();
        let env_snapshot: BTreeMap<String, String> = env::vars().collect();
        Self::from_argv_and_env(&argv, env_snapshot, cargo)
    }
}

impl OutputMode {
    /// Pick an [`OutputMode`] from an explicit user choice plus the
    /// ambient environment. `explicit` comes from `--output=` / `--plain`;
    /// `env` is the snapshot captured at startup (the `CI` key is used as
    /// a "definitely not a human terminal" hint even when stdout IS a
    /// TTY, because CI log capture frequently makes ANSI cursor-moves
    /// unreadable downstream).
    #[must_use]
    #[inline]
    pub fn resolve(explicit: Option<Self>, env: &BTreeMap<String, String>) -> Self {
        if let Some(mode) = explicit {
            return mode;
        }
        if io::stdout().is_terminal() && !env.contains_key("CI") {
            Self::Live
        } else {
            Self::Plain
        }
    }
}

/// Parse a `--ensure-time=<warn-ms>,<critical-ms>` value into thresholds.
///
/// Returns `None` for any malformed input — the caller falls back to
/// [`EnsureTimeArg::Default`] in that case so the flag still has effect
/// (matches libtest's lenient parse: a garbage `--ensure-time=foo` is
/// equivalent to a bare `--ensure-time`). Both halves must be unsigned
/// millisecond counts; pure-integer parse, no fractional ms.
fn parse_ensure_time_pair(text: &str) -> Option<EnsureTimeArg> {
    let (warn_text, critical_text) = text.split_once(',')?;
    let warn_ms: u64 = warn_text.parse().ok()?;
    let critical_ms: u64 = critical_text.parse().ok()?;
    Some(EnsureTimeArg::Explicit {
        warn: Duration::from_millis(warn_ms),
        critical: Duration::from_millis(critical_ms),
    })
}

/// Resolve `--ensure-time` to concrete warn/critical thresholds.
///
/// Resolution order (mirrors libtest):
/// 1. Inline `--ensure-time=<warn>,<critical>` value, if cleanly
///    parsed (`EnsureTimeArg::Explicit`).
/// 2. `RUST_TEST_TIME_INTEGRATION` env var with the same `<warn>,<critical>`
///    millisecond shape.
/// 3. [`EnsureTimeConfig::integration_defaults`] (500ms / 1000ms).
fn resolve_ensure_time(
    arg: Option<EnsureTimeArg>,
    env: &BTreeMap<String, String>,
) -> Option<EnsureTimeConfig> {
    let arg = arg?;
    match arg {
        EnsureTimeArg::Explicit { warn, critical } => Some(EnsureTimeConfig { warn, critical }),
        EnsureTimeArg::Default => {
            let env_value = env
                .get("RUST_TEST_TIME_INTEGRATION")
                .and_then(|raw| parse_ensure_time_pair(raw));
            match env_value {
                Some(EnsureTimeArg::Explicit { warn, critical }) => {
                    Some(EnsureTimeConfig { warn, critical })
                }
                _ => Some(EnsureTimeConfig::integration_defaults()),
            }
        }
    }
}

/// Map a `--threads-parallel-hardlimit=<value>` string to a [`HardlimitArg`].
fn classify_hardlimit(text: &str) -> Option<HardlimitArg> {
    match text {
        "none" => Some(HardlimitArg::Disabled),
        "threads" => Some(HardlimitArg::Threads),
        _ => text
            .parse::<usize>()
            .ok()
            .and_then(NonZeroUsize::new)
            .map(HardlimitArg::Explicit),
    }
}

/// Resolve the final parallel-hardlimit policy by combining a user's flag
/// choice with the active bench mode and the resolved thread count.
fn resolve_parallel_hardlimit(
    arg: HardlimitArg,
    bench_mode: BenchMode,
    threads_nz: NonZeroUsize,
) -> Option<NonZeroUsize> {
    match arg {
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
    }
}

/// Read the value belonging to a long flag whose syntax is either
/// `--flag=<value>` or `--flag <value>`. Returns the value (borrowed from
/// `argv`) and updates `i` to point at the value slot when the spaced form
/// is observed.
///
/// `prefix_eq` must include the trailing `=` (e.g. `"--test-threads="`);
/// `flag_name` is the bare form (e.g. `"--test-threads"`).
fn flag_value<'argv>(
    arg: &'argv str,
    flag_name: &str,
    prefix_eq: &str,
    argv: &'argv [String],
    i: &mut usize,
) -> Option<&'argv str> {
    if let Some(rest) = arg.strip_prefix(prefix_eq) {
        return Some(rest);
    }
    if arg == flag_name {
        *i = i.saturating_add(1);
        return argv.get(*i).map(String::as_str);
    }
    None
}

/// Parse a positive `usize` value from one of the `--flag=<n>` /
/// `--flag <n>` flags. Returns `Some(n)` only when the value is `>= 1`.
fn parse_positive_usize_flag(
    arg: &str,
    flag_name: &str,
    prefix_eq: &str,
    argv: &[String],
    i: &mut usize,
) -> Option<usize> {
    let value = flag_value(arg, flag_name, prefix_eq, argv, i)?;
    value.parse::<usize>().ok().filter(|&n| n > 0)
}

/// Parse a `--flag=<secs>` / `--flag <secs>` duration flag. Returns
/// `Some(Duration::from_secs(secs))` for any non-negative integer; an
/// explicit `0` here is preserved as `Duration::ZERO` (callers that want
/// "0 means disabled" should use [`parse_optional_duration_secs_flag`]).
fn parse_duration_secs_flag(
    arg: &str,
    flag_name: &str,
    prefix_eq: &str,
    argv: &[String],
    i: &mut usize,
) -> Option<Duration> {
    let value = flag_value(arg, flag_name, prefix_eq, argv, i)?;
    value.parse::<u64>().ok().map(Duration::from_secs)
}

/// Parse a `--flag=<secs>` / `--flag <secs>` duration flag where `secs == 0`
/// disables the feature.
///
/// Returns [`OptionalDurationParse::Set`] for a non-zero value,
/// [`OptionalDurationParse::Disabled`] for an explicit `0`, and
/// [`OptionalDurationParse::NotMatched`] when the flag wasn't matched at all
/// (or when its value didn't parse as `u64`).
fn parse_optional_duration_secs_flag(
    arg: &str,
    flag_name: &str,
    prefix_eq: &str,
    argv: &[String],
    i: &mut usize,
) -> OptionalDurationParse {
    let Some(value) = flag_value(arg, flag_name, prefix_eq, argv, i) else {
        return OptionalDurationParse::NotMatched;
    };
    let Ok(secs) = value.parse::<u64>() else {
        return OptionalDurationParse::NotMatched;
    };
    if secs == 0 {
        OptionalDurationParse::Disabled
    } else {
        OptionalDurationParse::Set(Duration::from_secs(secs))
    }
}

/// Try to consume a flag that selects a thread/concurrency knob. Returns
/// `true` when the current `arg` matched one of the knobs.
fn handle_concurrency_flag(
    state: &mut ParsedArgs,
    arg: &str,
    argv: &[String],
    i: &mut usize,
) -> bool {
    if let Some(n) = parse_positive_usize_flag(arg, "--test-threads", "--test-threads=", argv, i) {
        state.threads = Some(n);
        return true;
    }
    if let Some(n) =
        parse_positive_usize_flag(arg, "--concurrency-limit", "--concurrency-limit=", argv, i)
    {
        state.concurrency_limit = Some(n);
        return true;
    }
    if let Some(value) = flag_value(
        arg,
        "--threads-parallel-hardlimit",
        "--threads-parallel-hardlimit=",
        argv,
        i,
    ) {
        if let Some(parsed) = classify_hardlimit(value) {
            state.hardlimit_arg = parsed;
        }
        return true;
    }
    false
}

/// Parse a `ColorMode` from its CLI string spelling, defaulting unknown
/// values to [`ColorMode::Auto`].
fn color_mode_from_str(text: &str) -> ColorMode {
    match text {
        "always" => ColorMode::Always,
        "never" => ColorMode::Never,
        _ => ColorMode::Auto,
    }
}

/// Try to consume a flag that toggles a presentation knob (color, format,
/// output mode, list, help, ignored, bench). Returns `true` on match.
fn handle_presentation_flag(
    state: &mut ParsedArgs,
    arg: &str,
    argv: &[String],
    i: &mut usize,
) -> bool {
    if let Some(value) = flag_value(arg, "--color", "--color=", argv, i) {
        state.color = color_mode_from_str(value);
        return true;
    }
    if let Some(value) = flag_value(arg, "--format", "--format=", argv, i) {
        state.format = if value == "terse" {
            Format::Terse
        } else {
            Format::Pretty
        };
        return true;
    }
    if let Some(value) = flag_value(arg, "--output", "--output=", argv, i) {
        match value {
            "live" => state.output_mode_explicit = Some(OutputMode::Live),
            "plain" => state.output_mode_explicit = Some(OutputMode::Plain),
            _ => {}
        }
        return true;
    }
    if let Some(value) = flag_value(arg, "--logfile", "--logfile=", argv, i) {
        state.logfile = Some(PathBuf::from(value));
        return true;
    }
    // `--report-time` is libtest's per-test elapsed-time switch. Rudzio
    // already prints elapsed for every test in the default pretty
    // output (the `<runtime, 142ms>` block), so the flag is implicitly
    // satisfied; accept and discard rather than letting it land in
    // `unparsed` where it would emit a "we don't recognise this" notice.
    // Recorded in `compat_consumed` so debug tooling can see what was
    // accepted-as-default rather than acted on.
    if arg == "--report-time" {
        state.compat_consumed.push(arg.to_owned());
        return true;
    }
    // `--ensure-time [=<WARN-MS>,<CRIT-MS>]` is libtest's tiered
    // wall-clock gate: tests over `warn` log a notice; tests over
    // `critical` are counted as failures (and bump the exit code).
    // Bare form defers warn/critical to `RUST_TEST_TIME_INTEGRATION`
    // (then libtest's 500/1000 ms integration defaults). Inline value
    // bypasses the env. Libtest only spells this flag in the `=value`
    // form (or bare) — never as a separate value arg — so we don't peek
    // at the next argv entry here.
    if arg == "--ensure-time" {
        state.ensure_time = Some(EnsureTimeArg::Default);
        return true;
    }
    if let Some(value) = arg.strip_prefix("--ensure-time=") {
        state.ensure_time = Some(parse_ensure_time_pair(value).unwrap_or(EnsureTimeArg::Default));
        return true;
    }
    if let Some(value) = flag_value(arg, "--shuffle-seed", "--shuffle-seed=", argv, i) {
        if let Ok(seed) = value.parse::<u64>() {
            state.shuffle_seed = Some(seed);
            state.shuffle = true;
        }
        // Garbage value: leave shuffle off, leave seed None — matches
        // how libtest treats an unparsable seed (silent fallthrough).
        return true;
    }
    if arg == "--shuffle" {
        state.shuffle = true;
        return true;
    }
    match arg {
        "--ignored" => state.run_ignored = RunIgnoredMode::Only,
        "--include-ignored" => state.run_ignored = RunIgnoredMode::Include,
        "--bench" => state.bench_mode = BenchMode::Full,
        "--no-bench" => state.bench_mode = BenchMode::Skip,
        "--exact" => state.exact_match = true,
        "--quiet" | "-q" => state.format = Format::Terse,
        "--plain" => state.output_mode_explicit = Some(OutputMode::Plain),
        "--list" => state.list = true,
        "--help" | "-h" => state.help = true,
        _ => return false,
    }
    true
}

/// Try to consume one of the `Option<Duration>` timeout flags. Returns
/// `true` on match.
fn handle_timeout_flag(state: &mut ParsedArgs, arg: &str, argv: &[String], i: &mut usize) -> bool {
    if let Some(duration) =
        parse_duration_secs_flag(arg, "--test-timeout", "--test-timeout=", argv, i)
    {
        state.test_timeout = Some(duration);
        return true;
    }
    if let Some(duration) =
        parse_duration_secs_flag(arg, "--run-timeout", "--run-timeout=", argv, i)
    {
        state.run_timeout = Some(duration);
        return true;
    }
    if let Some(duration) = parse_duration_secs_flag(
        arg,
        "--suite-setup-timeout",
        "--suite-setup-timeout=",
        argv,
        i,
    ) {
        state.suite_setup_timeout = Some(duration);
        return true;
    }
    if let Some(duration) = parse_duration_secs_flag(
        arg,
        "--suite-teardown-timeout",
        "--suite-teardown-timeout=",
        argv,
        i,
    ) {
        state.suite_teardown_timeout = Some(duration);
        return true;
    }
    if let Some(duration) = parse_duration_secs_flag(
        arg,
        "--test-setup-timeout",
        "--test-setup-timeout=",
        argv,
        i,
    ) {
        state.test_setup_timeout = Some(duration);
        return true;
    }
    if let Some(duration) = parse_duration_secs_flag(
        arg,
        "--test-teardown-timeout",
        "--test-teardown-timeout=",
        argv,
        i,
    ) {
        state.test_teardown_timeout = Some(duration);
        return true;
    }
    match parse_optional_duration_secs_flag(
        arg,
        "--phase-hang-grace",
        "--phase-hang-grace=",
        argv,
        i,
    ) {
        OptionalDurationParse::Set(duration) => {
            state.phase_hang_grace = Some(duration);
            return true;
        }
        OptionalDurationParse::Disabled => {
            state.phase_hang_grace = None;
            return true;
        }
        OptionalDurationParse::NotMatched => {}
    }
    match parse_optional_duration_secs_flag(
        arg,
        "--cancel-grace-period",
        "--cancel-grace-period=",
        argv,
        i,
    ) {
        OptionalDurationParse::Set(duration) => {
            state.cancel_grace_period = Some(duration);
            return true;
        }
        OptionalDurationParse::Disabled => {
            state.cancel_grace_period = None;
            return true;
        }
        OptionalDurationParse::NotMatched => {}
    }
    false
}

/// Try to consume a `--skip` filter, the implicit positional filter, or
/// fall back to recording the arg in `unparsed`.
fn handle_filter_or_unparsed(
    state: &mut ParsedArgs,
    arg: &str,
    argv: &[String],
    i: &mut usize,
) -> bool {
    if let Some(rest) = arg.strip_prefix("--skip=") {
        state.skip_filters.push(rest.to_owned());
        return true;
    }
    if arg == "--skip" {
        *i = i.saturating_add(1);
        if let Some(next) = argv.get(*i) {
            state.skip_filters.push(next.clone());
        }
        return true;
    }
    if arg.starts_with('-') {
        state.unparsed.push(arg.to_owned());
    } else {
        state.filter = Some(arg.to_owned());
    }
    true
}

/// Run the full argv pass, handing each arg to the family-specific
/// helpers in turn. Returns the populated [`ParsedArgs`] for downstream
/// resolution.
fn parse_argv(argv: &[String]) -> ParsedArgs {
    // Layer-1 process-exit grace defaults to 5s; sync-blocked
    // tasks ignoring SIGINT have always-on protection from
    // `process::exit(2)` so the binary can't hang forever.
    let mut state = ParsedArgs {
        cancel_grace_period: Some(Duration::from_secs(5)),
        ..ParsedArgs::default()
    };

    let mut i = 0_usize;
    while i < argv.len() {
        let Some(arg) = argv.get(i).cloned() else {
            break;
        };
        let consumed = handle_concurrency_flag(&mut state, &arg, argv, &mut i)
            || handle_presentation_flag(&mut state, &arg, argv, &mut i)
            || handle_timeout_flag(&mut state, &arg, argv, &mut i)
            || handle_filter_or_unparsed(&mut state, &arg, argv, &mut i);
        debug_assert!(consumed, "every arg should be consumed by some handler");
        i = i.saturating_add(1);
    }

    state
}
