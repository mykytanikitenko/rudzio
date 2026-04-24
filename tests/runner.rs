//! Rudzio's own dogfood test suite.
//!
//! Lives as a regular integration test (so per-crate `cargo test -p rudzio`
//! just works) AND is re-compiled into the auto-generated workspace-wide
//! aggregator (`<target-dir>/rudzio-auto-runner/`) via `#[path]` when run
//! under `cargo rudzio test`. That dual-role layout keeps Cargo's workspace
//! feature unification from leaking the `tests` feature into e2e fixture
//! binaries.
//!
//! The `Config` parser is pure-sync, so exercising it under every runtime
//! rudzio ships is cheap — and doubles as a POC that a single
//! `#[rudzio::suite]` block can target multiple runtimes.

use std::collections::BTreeMap;

use rudzio::Config;

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn env_with(rust_test_threads: Option<&str>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(v) = rust_test_threads {
        let _inserted = env.insert("RUST_TEST_THREADS".to_owned(), v.to_owned());
    }
    env
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::Local::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::embassy::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::futures::ThreadPool::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod config_parser {
    use super::{Config, argv, env_with};
    use rudzio::common::context::Test;

    // `#[rudzio::test]` accepts fns that don't take a context parameter
    // at all. The runner still creates the per-test context + runs its
    // teardown; the body just doesn't see it.
    #[rudzio::test]
    fn body_without_ctx_parameter() -> anyhow::Result<()> {
        // Pure-sync test with no ctx — exercises the `CtxKind::None`
        // codegen branch.
        anyhow::ensure!(1 + 1 == 2);
        Ok(())
    }

    #[rudzio::test]
    async fn async_body_without_ctx_parameter() -> anyhow::Result<()> {
        anyhow::ensure!(1 + 1 == 2);
        Ok(())
    }

    #[rudzio::test]
    fn joined_argv_form_is_parsed(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=4"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads == 4, "threads = {}", c.threads);
        Ok(())
    }

    #[rudzio::test]
    fn split_argv_form_is_parsed(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads", "8"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads == 8);
        Ok(())
    }

    #[rudzio::test]
    fn env_var_alone_is_used(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&[]), env_with(Some("3")), rudzio::cargo_meta!());
        anyhow::ensure!(c.threads == 3);
        Ok(())
    }

    #[rudzio::test]
    fn argv_takes_precedence_over_env(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=2"]),
            env_with(Some("7")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads == 2);
        Ok(())
    }

    #[rudzio::test]
    fn zero_threads_falls_through_to_available_parallelism(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=0"]),
            env_with(Some("0")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn garbage_threads_falls_through(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=abc"]),
            env_with(Some("xyz")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn zero_in_env_is_ignored_when_argv_is_valid(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=5"]),
            env_with(Some("0")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads == 5);
        Ok(())
    }

    #[rudzio::test]
    fn unknown_flags_are_preserved_in_unparsed(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&[
                "--nocapture",
                "--color=always",
                "--test-threads=3",
                "--format=json",
            ]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads == 3);
        anyhow::ensure!(
            c.unparsed.iter().any(|s| s == "--nocapture"),
            "unparsed = {:?}",
            c.unparsed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn split_form_without_value_falls_through(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn both_unset_uses_available_parallelism(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn filter_substring_is_captured(_ctx: &Test) -> anyhow::Result<()> {
        let c =
            Config::from_argv_and_env(argv(&["my_filter"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(c.filter.as_deref() == Some("my_filter"));
        Ok(())
    }

    #[rudzio::test]
    fn skip_filters_accumulate(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--skip=foo", "--skip", "bar"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.skip_filters == vec!["foo".to_owned(), "bar".to_owned()]);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_defaults_to_threads(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=4"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads == 4);
        anyhow::ensure!(c.concurrency_limit == 4);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_is_independent_when_set(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--concurrency-limit=2"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.threads == 8);
        anyhow::ensure!(c.concurrency_limit == 2);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_split_form(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--concurrency-limit", "3"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.concurrency_limit == 3);
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_defaults_to_threads(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            c.parallel_hardlimit.map(std::num::NonZeroUsize::get) == Some(8),
            "expected Some(8), got {:?}",
            c.parallel_hardlimit
        );
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_equals_form(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit=3"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.map(std::num::NonZeroUsize::get) == Some(3));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_split_form(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit", "3"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.map(std::num::NonZeroUsize::get) == Some(3));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_none_disables_equals_form(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit=none"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.is_none());
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_none_disables_split_form(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit", "none"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.is_none());
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_threads_keyword(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit=threads"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.map(std::num::NonZeroUsize::get) == Some(8));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_zero_falls_back_to_default(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit=0"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.map(std::num::NonZeroUsize::get) == Some(8));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_invalid_falls_back_to_default(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit=foo"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.map(std::num::NonZeroUsize::get) == Some(8));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_bench_auto_disables_when_unset(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--bench"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            c.parallel_hardlimit.is_none(),
            "expected None under --bench with no explicit flag, got {:?}",
            c.parallel_hardlimit
        );
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_explicit_survives_bench(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&[
                "--test-threads=8",
                "--bench",
                "--threads-parallel-hardlimit=4",
            ]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.map(std::num::NonZeroUsize::get) == Some(4));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_explicit_none_survives_non_bench(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--threads-parallel-hardlimit=none"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(c.parallel_hardlimit.is_none());
        Ok(())
    }

    #[rudzio::test]
    fn env_is_propagated_into_config(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&[]), env_with(Some("4")), rudzio::cargo_meta!());
        anyhow::ensure!(c.env.get("RUST_TEST_THREADS").map(String::as_str) == Some("4"));
        Ok(())
    }

    #[rudzio::test]
    fn bench_mode_defaults_to_smoke(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(c.bench_mode == rudzio::BenchMode::Smoke);
        Ok(())
    }

    #[rudzio::test]
    fn bench_flag_sets_full_mode(_ctx: &Test) -> anyhow::Result<()> {
        let c =
            Config::from_argv_and_env(argv(&["--bench"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(c.bench_mode == rudzio::BenchMode::Full);
        Ok(())
    }

    #[rudzio::test]
    fn no_bench_flag_sets_skip_mode(_ctx: &Test) -> anyhow::Result<()> {
        let c =
            Config::from_argv_and_env(argv(&["--no-bench"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(c.bench_mode == rudzio::BenchMode::Skip);
        Ok(())
    }
}

/// Strategy-level smoke tests dogfooded across every runtime rudzio
/// ships. The stock strategies only need "poll this future" and
/// `futures::join_all`, so they're independent of the runtime's
/// concurrency model — proving that on every backend is a cheap POC.
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::Local::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::embassy::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::futures::ThreadPool::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod bench_strategies {
    use rudzio::bench::{
        BenchReport, Strategy,
        strategy::{Concurrent, Sequential},
    };
    use rudzio::common::context::Test;

    #[rudzio::test]
    async fn sequential_runs_body_n_times(_ctx: &Test) -> anyhow::Result<()> {
        let count = std::sync::atomic::AtomicUsize::new(0);
        let report: BenchReport = Sequential(7)
            .run(|| async {
                let _prev = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await;
        anyhow::ensure!(count.load(std::sync::atomic::Ordering::SeqCst) == 7);
        anyhow::ensure!(report.iterations == 7, "iterations = {}", report.iterations);
        anyhow::ensure!(report.samples.len() == 7);
        anyhow::ensure!(report.failures.is_empty());
        anyhow::ensure!(report.panics == 0);
        anyhow::ensure!(report.is_success());
        anyhow::ensure!(report.strategy == "Sequential(7)");
        Ok(())
    }

    #[rudzio::test]
    async fn concurrent_runs_body_n_times(_ctx: &Test) -> anyhow::Result<()> {
        let count = std::sync::atomic::AtomicUsize::new(0);
        let report: BenchReport = Concurrent(5)
            .run(|| async {
                let _prev = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await;
        anyhow::ensure!(count.load(std::sync::atomic::Ordering::SeqCst) == 5);
        anyhow::ensure!(report.iterations == 5);
        anyhow::ensure!(report.samples.len() == 5);
        anyhow::ensure!(report.is_success());
        anyhow::ensure!(report.strategy == "Concurrent(5)");
        Ok(())
    }

    #[rudzio::test]
    async fn sequential_captures_failures(_ctx: &Test) -> anyhow::Result<()> {
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let report = Sequential(4)
            .run(|| async {
                let i = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i % 2 == 0 {
                    Ok(())
                } else {
                    Err(rudzio::test_case::box_error("even iteration required"))
                }
            })
            .await;
        anyhow::ensure!(report.samples.len() == 2);
        anyhow::ensure!(report.failures.len() == 2);
        anyhow::ensure!(!report.is_success());
        Ok(())
    }

    #[rudzio::test]
    async fn empty_samples_return_none_for_stats(_ctx: &Test) -> anyhow::Result<()> {
        let report = Sequential(0)
            .run(|| async { Ok::<(), rudzio::test_case::BoxError>(()) })
            .await;
        anyhow::ensure!(report.min().is_none());
        anyhow::ensure!(report.max().is_none());
        anyhow::ensure!(report.mean().is_none());
        anyhow::ensure!(report.median().is_none());
        anyhow::ensure!(report.percentile(0.5).is_none());
        anyhow::ensure!(report.ascii_histogram(8, 20).is_empty());
        Ok(())
    }

    #[rudzio::test]
    async fn percentile_rejects_out_of_range(_ctx: &Test) -> anyhow::Result<()> {
        let report = Sequential(3)
            .run(|| async { Ok::<(), rudzio::test_case::BoxError>(()) })
            .await;
        anyhow::ensure!(report.percentile(-0.1).is_none());
        anyhow::ensure!(report.percentile(1.01).is_none());
        anyhow::ensure!(report.percentile(0.0).is_some());
        anyhow::ensure!(report.percentile(1.0).is_some());
        Ok(())
    }

    // End-to-end: a bench-annotated test. Under `cargo test` this runs
    // once as a smoke test (body invoked exactly once, no stats
    // collection). Under `cargo test -- --bench` it runs with the
    // strategy. The iteration count stays tiny so the smoke path
    // doesn't dominate the runtime sweep.
    #[rudzio::test(benchmark = rudzio::bench::strategy::Sequential(3))]
    async fn sample_sequential_bench(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test(benchmark = rudzio::bench::strategy::Concurrent(3))]
    async fn sample_concurrent_bench(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    // Bench tests without a context parameter also work — setup and
    // teardown still run around the strategy invocation.
    #[rudzio::test(benchmark = rudzio::bench::strategy::Sequential(2))]
    async fn sample_bench_without_ctx() -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::Local::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::embassy::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::futures::ThreadPool::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod build_sentinel {
    use std::ffi::OsStr;

    use rudzio::build::{
        NESTED_SENTINEL_ENV, SentinelAction, decide_sentinel_action,
        sentinel_indicates_nested_call, strip_rudzio_test_cfg,
    };

    #[rudzio::test]
    fn no_sentinel_means_proceed() -> anyhow::Result<()> {
        anyhow::ensure!(
            decide_sentinel_action(None, "anything", "this-crate") == SentinelAction::Proceed
        );
        anyhow::ensure!(
            decide_sentinel_action(None, "this-crate", "this-crate") == SentinelAction::Proceed
        );
        Ok(())
    }

    #[rudzio::test]
    fn empty_sentinel_means_proceed() -> anyhow::Result<()> {
        anyhow::ensure!(
            decide_sentinel_action(Some(OsStr::new("")), "x", "x") == SentinelAction::Proceed
        );
        Ok(())
    }

    #[rudzio::test]
    fn sentinel_set_same_crate_silent_ok() -> anyhow::Result<()> {
        anyhow::ensure!(
            decide_sentinel_action(Some(OsStr::new("1")), "file-v3", "file-v3")
                == SentinelAction::SilentOk
        );
        Ok(())
    }

    #[rudzio::test]
    fn sentinel_set_different_crate_warn_and_ok() -> anyhow::Result<()> {
        anyhow::ensure!(
            decide_sentinel_action(Some(OsStr::new("1")), "A", "B") == SentinelAction::WarnAndOk
        );
        Ok(())
    }

    #[rudzio::test]
    fn sentinel_detector_recognises_any_non_empty_value() -> anyhow::Result<()> {
        anyhow::ensure!(sentinel_indicates_nested_call(Some(OsStr::new("1"))));
        anyhow::ensure!(sentinel_indicates_nested_call(Some(OsStr::new("yes"))));
        anyhow::ensure!(sentinel_indicates_nested_call(Some(OsStr::new("0"))));
        Ok(())
    }

    #[rudzio::test]
    fn sentinel_detector_ignores_absent_or_empty() -> anyhow::Result<()> {
        anyhow::ensure!(!sentinel_indicates_nested_call(None));
        anyhow::ensure!(!sentinel_indicates_nested_call(Some(OsStr::new(""))));
        Ok(())
    }

    #[rudzio::test]
    fn strip_rudzio_test_cfg_removes_pair_and_preserves_other_flags() -> anyhow::Result<()> {
        anyhow::ensure!(strip_rudzio_test_cfg("--cfg rudzio_test") == "");
        anyhow::ensure!(
            strip_rudzio_test_cfg("-C opt-level=1 --cfg rudzio_test --cfg foo")
                == "-C opt-level=1 --cfg foo",
        );
        anyhow::ensure!(strip_rudzio_test_cfg("-C debuginfo=2") == "-C debuginfo=2");
        anyhow::ensure!(strip_rudzio_test_cfg("") == "");
        anyhow::ensure!(
            strip_rudzio_test_cfg("--cfg rudzio_test_helper --cfg rudzio_test")
                == "--cfg rudzio_test_helper",
        );
        Ok(())
    }

    #[rudzio::test]
    fn sentinel_env_name_is_pinned() -> anyhow::Result<()> {
        // Contract: cargo-rudzio sets this env var before spawning cargo
        // so bridge-forwarded build.rs calls to expose_self_bins detect
        // re-entry and early-return. The literal is duplicated in
        // cargo-rudzio/src/lib.rs::EXPOSE_BINS_SENTINEL_ENV — if one
        // side drifts, the other must follow or bridges regress to
        // "no [[bin]] targets" errors under cargo rudzio test.
        anyhow::ensure!(
            NESTED_SENTINEL_ENV == "__RUDZIO_EXPOSE_BINS_ACTIVE",
            "sentinel env-var name drift: {NESTED_SENTINEL_ENV}"
        );
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::Local::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::embassy::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::futures::ThreadPool::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod bin_resolver {
    use rudzio::bin::__resolve_at_runtime;

    #[rudzio::test]
    fn runtime_walk_reaches_a_directory_that_exists() -> anyhow::Result<()> {
        let current = std::env::current_exe()?;
        let profile_dir = current
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| anyhow::anyhow!("test binary has no grandparent dir"))?;
        anyhow::ensure!(
            profile_dir.is_dir(),
            "expected `{}` to be a real directory (usually `.../target/<profile>/`)",
            profile_dir.display(),
        );
        Ok(())
    }

    #[rudzio::test]
    fn missing_bin_error_names_the_bin_and_suggests_fixes() -> anyhow::Result<()> {
        let err = __resolve_at_runtime("this-bin-definitely-does-not-exist-xyz-123")
            .expect_err("bogus bin name must not resolve");
        let msg = err.to_string();
        anyhow::ensure!(
            msg.contains("this-bin-definitely-does-not-exist-xyz-123"),
            "error must name the bin so the user sees what was looked up; got: {msg}",
        );
        anyhow::ensure!(
            msg.contains("cargo build --bins"),
            "error should point at `cargo build --bins` as a fix; got: {msg}",
        );
        anyhow::ensure!(
            msg.contains("expose_self_bins"),
            "error should point at `expose_self_bins` as a fix; got: {msg}",
        );
        Ok(())
    }
}
