//! Resolved CLI / environment configuration for a test run.
//!
//! [`Config`] is built once per invocation by [`Config::parse`] (reading
//! `env::args()` and every environment variable at startup) and then passed
//! to every runtime constructor, every suite setup, and — via
//! [`crate::runtime::Runtime::config`] — to any downstream code that wants
//! to inspect the flags the test binary was launched with.

use std::collections::BTreeMap;
use std::env;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

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
    /// Output format.
    pub format: Format,
    /// Colour policy.
    pub color: ColorMode,
    /// How `#[ignore]`d tests are treated.
    pub run_ignored: RunIgnoredMode,
    /// How `#[rudzio::test(benchmark = ...)]`-annotated tests are treated.
    pub bench_mode: BenchMode,
    /// `--list`: print test names and exit without running.
    pub list: bool,
    /// `--test-timeout=<secs>`. `None` = unbounded.
    pub test_timeout: Option<Duration>,
    /// `--run-timeout=<secs>`. `None` = unbounded.
    pub run_timeout: Option<Duration>,
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
        let mut filter: Option<String> = None;
        let mut skip_filters: Vec<String> = Vec::new();
        let mut threads: Option<usize> = None;
        let mut concurrency_limit: Option<usize> = None;
        let mut format = Format::Pretty;
        let mut color = ColorMode::Auto;
        let mut run_ignored = RunIgnoredMode::Normal;
        let mut bench_mode = BenchMode::Smoke;
        let mut list = false;
        let mut test_timeout: Option<Duration> = None;
        let mut run_timeout: Option<Duration> = None;
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

        Self {
            filter,
            skip_filters,
            threads,
            concurrency_limit,
            format,
            color,
            run_ignored,
            bench_mode,
            list,
            test_timeout,
            run_timeout,
            unparsed,
            env,
            cargo,
        }
    }
}
