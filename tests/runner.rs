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
use std::env::current_exe;
use std::ffi::OsStr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rudzio::bench::Report;
use rudzio::bench::strategy::{Concurrent, Sequential};
use rudzio::bin::__resolve_at_runtime;
use rudzio::build::{
    NESTED_SENTINEL_ENV, SentinelAction, decide_sentinel_action, sentinel_indicates_nested_call,
};
use rudzio::common::context::{Suite, Test};
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
use rudzio::suite::SummaryOutcomes;
use rudzio::test_case::{BoxError, box_error};
use rudzio::{
    BenchMode, Config, EnsureTimeViolation, EnsureTimes, Format, RunIgnoredMode, SuiteSummary,
    TestSummary, normalize_module_path, qualified_test_name, token_passes_filters,
};

/// Build a `Vec<String>` argv from string slices — the `Config`
/// parser takes owned strings (matches `std::env::args()`'s shape),
/// so tests need a small helper to materialise them from `&str`.
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Build a `BTreeMap` for the env half of `Config::from_argv_and_env`,
/// optionally pre-populating `RUST_TEST_THREADS` so a test can pin
/// the env-var contribution without polluting the real process env.
fn env_with(rust_test_threads: Option<&str>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(value) = rust_test_threads {
        let _inserted = env.insert("RUST_TEST_THREADS".to_owned(), value.to_owned());
    }
    env
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod config_parser {
    use std::env::temp_dir;
    use std::fs;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudzio::output::logfile::Writer as LogfileWriter;
    use rudzio::shuffle::permute_with_seed;

    use super::{
        BenchMode, Config, Duration, EnsureTimeViolation, EnsureTimes, Format, NonZeroUsize,
        PathBuf, SuiteSummary, SummaryOutcomes, Test, TestSummary, argv, env_with,
    };

    // `#[rudzio::test]` accepts fns that don't take a context parameter
    // at all. The runner still creates the per-test context + runs its
    // teardown; the body just doesn't see it.
    #[rudzio::test]
    fn body_without_ctx_parameter() -> anyhow::Result<()> {
        // Pure-sync test with no ctx — exercises the `CtxKind::None`
        // codegen branch.
        anyhow::ensure!(1_i32 + 1_i32 == 2_i32);
        Ok(())
    }

    #[rudzio::test]
    async fn async_body_without_ctx_parameter() -> anyhow::Result<()> {
        anyhow::ensure!(1_i32 + 1_i32 == 2_i32);
        Ok(())
    }

    #[rudzio::test]
    fn joined_argv_form_is_parsed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=4"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads == 4, "threads = {}", cfg.threads);
        Ok(())
    }

    #[rudzio::test]
    fn split_argv_form_is_parsed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads", "8"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads == 8);
        Ok(())
    }

    #[rudzio::test]
    fn env_var_alone_is_used(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(Some("3")), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.threads == 3);
        Ok(())
    }

    #[rudzio::test]
    fn argv_takes_precedence_over_env(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=2"]),
            env_with(Some("7")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads == 2);
        Ok(())
    }

    #[rudzio::test]
    fn zero_threads_falls_through_to_available_parallelism(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=0"]),
            env_with(Some("0")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn garbage_threads_falls_through(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=abc"]),
            env_with(Some("xyz")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn zero_in_env_is_ignored_when_argv_is_valid(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=5"]),
            env_with(Some("0")),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads == 5);
        Ok(())
    }

    #[rudzio::test]
    fn unknown_flags_are_preserved_in_unparsed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&[
                "--nocapture",
                "--color=always",
                "--test-threads=3",
                "--format=json",
            ]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads == 3);
        anyhow::ensure!(
            cfg.unparsed.iter().any(|item| item == "--nocapture"),
            "unparsed = {:?}",
            cfg.unparsed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn split_form_without_value_falls_through(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn both_unset_uses_available_parallelism(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn filter_substring_is_captured(_ctx: &Test) -> anyhow::Result<()> {
        let cfg =
            Config::from_argv_and_env(&argv(&["my_filter"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.filter.as_deref() == Some("my_filter"));
        Ok(())
    }

    #[rudzio::test]
    fn logfile_absent_is_none(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.logfile.is_none(), "logfile = {:?}", cfg.logfile);
        Ok(())
    }

    #[rudzio::test]
    fn logfile_space_form_is_captured(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--logfile", "out.log"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.logfile.as_deref() == Some(PathBuf::from("out.log").as_path()),
            "logfile = {:?}",
            cfg.logfile,
        );
        Ok(())
    }

    #[rudzio::test]
    fn logfile_equals_form_is_captured(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--logfile=out.log"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.logfile.as_deref() == Some(PathBuf::from("out.log").as_path()),
            "logfile = {:?}",
            cfg.logfile,
        );
        Ok(())
    }

    #[rudzio::test]
    fn logfile_does_not_leak_into_filter(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--logfile", "out.log", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.filter.as_deref() == Some("my_filter"));
        anyhow::ensure!(
            cfg.logfile.as_deref() == Some(PathBuf::from("out.log").as_path()),
            "logfile = {:?}",
            cfg.logfile,
        );
        Ok(())
    }

    #[rudzio::test]
    fn logfile_writer_disabled_when_no_path(_ctx: &Test) -> anyhow::Result<()> {
        let writer = LogfileWriter::open(None);
        writer.write_line("ok", "any::name");
        anyhow::ensure!(
            !writer.is_enabled(),
            "writer should be a no-op when path is None"
        );
        Ok(())
    }

    #[rudzio::test]
    fn logfile_writer_emits_libtest_format_lines(_ctx: &Test) -> anyhow::Result<()> {
        let path = temp_dir().join(format!(
            "rudzio-logfile-emits-{}-{}.log",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
        ));
        let writer = LogfileWriter::open(Some(&path));
        anyhow::ensure!(
            writer.is_enabled(),
            "writer should be enabled when path opens"
        );
        writer.write_line("ok", "foo::bar");
        writer.write_line("failed", "foo::baz");
        writer.write_line("ignored", "foo::qux");
        writer.flush();
        let contents = fs::read_to_string(&path)?;
        let _removed = fs::remove_file(&path);
        anyhow::ensure!(
            contents == "ok foo::bar\nfailed foo::baz\nignored foo::qux\n",
            "got {contents:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_flag_defaults_to_off(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(!cfg.shuffle, "shuffle should default to false");
        anyhow::ensure!(
            cfg.shuffle_seed.is_none(),
            "shuffle_seed should default to None, got {:?}",
            cfg.shuffle_seed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_flag_alone_enables_shuffle_without_seed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg =
            Config::from_argv_and_env(&argv(&["--shuffle"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.shuffle, "shuffle should be true");
        anyhow::ensure!(
            cfg.shuffle_seed.is_none(),
            "shuffle_seed should still be None without --shuffle-seed",
        );
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_seed_equals_form_implies_shuffle(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--shuffle-seed=42"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.shuffle, "shuffle should be implicitly true");
        anyhow::ensure!(
            cfg.shuffle_seed == Some(42),
            "shuffle_seed = {:?}",
            cfg.shuffle_seed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_seed_space_form_implies_shuffle(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--shuffle-seed", "1234567890"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.shuffle);
        anyhow::ensure!(cfg.shuffle_seed == Some(1_234_567_890));
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_with_seed_is_deterministic(_ctx: &Test) -> anyhow::Result<()> {
        let mut first: Vec<u32> = (0..50_u32).collect();
        let mut second: Vec<u32> = (0..50_u32).collect();
        permute_with_seed(&mut first, 12345);
        permute_with_seed(&mut second, 12345);
        anyhow::ensure!(first == second, "same seed must yield same permutation");
        anyhow::ensure!(
            first != (0..50_u32).collect::<Vec<_>>(),
            "shuffle should change the order with overwhelming probability",
        );
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_with_different_seeds_yields_different_orders(_ctx: &Test) -> anyhow::Result<()> {
        let mut first: Vec<u32> = (0..50_u32).collect();
        let mut second: Vec<u32> = (0..50_u32).collect();
        permute_with_seed(&mut first, 1);
        permute_with_seed(&mut second, 2);
        anyhow::ensure!(
            first != second,
            "different seeds should permute differently"
        );
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_preserves_multiset(_ctx: &Test) -> anyhow::Result<()> {
        let original: Vec<u32> = (0..100_u32).collect();
        let mut permuted = original.clone();
        permute_with_seed(&mut permuted, 0xDEAD_BEEF);
        let mut sorted = permuted.clone();
        sorted.sort_unstable();
        anyhow::ensure!(sorted == original, "shuffle must be a permutation");
        Ok(())
    }

    #[rudzio::test]
    fn shuffle_seed_garbage_value_falls_through(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--shuffle-seed=not-a-number", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            !cfg.shuffle,
            "garbage shuffle-seed value should not enable shuffle",
        );
        anyhow::ensure!(cfg.shuffle_seed.is_none());
        Ok(())
    }

    #[rudzio::test]
    fn report_time_flag_is_silently_consumed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--report-time", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.filter.as_deref() == Some("my_filter"));
        anyhow::ensure!(
            !cfg.unparsed.iter().any(|item| item == "--report-time"),
            "--report-time should not leak into unparsed: {:?}",
            cfg.unparsed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn compat_consumed_records_report_time(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--report-time", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.compat_consumed == vec!["--report-time".to_owned()],
            "got {:?}",
            cfg.compat_consumed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn compat_consumed_empty_when_no_silent_compat_flags(_ctx: &Test) -> anyhow::Result<()> {
        let cfg =
            Config::from_argv_and_env(&argv(&["my_filter"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(
            cfg.compat_consumed.is_empty(),
            "got {:?}",
            cfg.compat_consumed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn compat_consumed_preserves_order_for_duplicates(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--report-time", "my_filter", "--report-time"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.compat_consumed == vec!["--report-time".to_owned(), "--report-time".to_owned()],
            "got {:?}",
            cfg.compat_consumed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_flag_bare_is_silently_consumed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--ensure-time", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.filter.as_deref() == Some("my_filter"));
        anyhow::ensure!(
            !cfg.unparsed.iter().any(|item| item == "--ensure-time"),
            "--ensure-time should not leak into unparsed: {:?}",
            cfg.unparsed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_flag_with_value_is_silently_consumed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--ensure-time=1,5", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.filter.as_deref() == Some("my_filter"));
        anyhow::ensure!(
            !cfg.unparsed
                .iter()
                .any(|item| item.starts_with("--ensure-time")),
            "--ensure-time=… should not leak into unparsed: {:?}",
            cfg.unparsed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_absent_means_none(_ctx: &Test) -> anyhow::Result<()> {
        let cfg =
            Config::from_argv_and_env(&argv(&["my_filter"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(
            cfg.ensure_time.is_none(),
            "expected None, got {:?}",
            cfg.ensure_time,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_bare_uses_libtest_integration_defaults(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--ensure-time", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        let thresholds = cfg
            .ensure_time
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected Some, got None"))?;
        anyhow::ensure!(
            thresholds.warn == Duration::from_millis(500),
            "warn = {:?}",
            thresholds.warn,
        );
        anyhow::ensure!(
            thresholds.critical == Duration::from_secs(1),
            "critical = {:?}",
            thresholds.critical,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_bare_honors_rust_test_time_integration_env(_ctx: &Test) -> anyhow::Result<()> {
        let mut env = env_with(None);
        let _prev = env.insert(
            "RUST_TEST_TIME_INTEGRATION".to_owned(),
            "120,250".to_owned(),
        );
        let cfg = Config::from_argv_and_env(
            &argv(&["--ensure-time", "my_filter"]),
            env,
            rudzio::cargo_meta!(),
        );
        let thresholds = cfg
            .ensure_time
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected Some, got None"))?;
        anyhow::ensure!(
            thresholds.warn == Duration::from_millis(120),
            "warn = {:?}",
            thresholds.warn,
        );
        anyhow::ensure!(
            thresholds.critical == Duration::from_millis(250),
            "critical = {:?}",
            thresholds.critical,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_explicit_value_overrides_env(_ctx: &Test) -> anyhow::Result<()> {
        let mut env = env_with(None);
        let _prev = env.insert(
            "RUST_TEST_TIME_INTEGRATION".to_owned(),
            "999,9999".to_owned(),
        );
        let cfg = Config::from_argv_and_env(
            &argv(&["--ensure-time=42,84", "my_filter"]),
            env,
            rudzio::cargo_meta!(),
        );
        let thresholds = cfg
            .ensure_time
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected Some, got None"))?;
        anyhow::ensure!(
            thresholds.warn == Duration::from_millis(42),
            "warn = {:?}",
            thresholds.warn,
        );
        anyhow::ensure!(
            thresholds.critical == Duration::from_millis(84),
            "critical = {:?}",
            thresholds.critical,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_garbage_value_falls_back_to_defaults(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--ensure-time=not-a-pair", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        let thresholds = cfg
            .ensure_time
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected Some, got None"))?;
        anyhow::ensure!(
            thresholds.warn == Duration::from_millis(500),
            "warn = {:?}",
            thresholds.warn,
        );
        anyhow::ensure!(
            thresholds.critical == Duration::from_secs(1),
            "critical = {:?}",
            thresholds.critical,
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_violation_below_warn_returns_none(_ctx: &Test) -> anyhow::Result<()> {
        let thresholds = EnsureTimes::integration_defaults();
        let outcome = thresholds.violation(Duration::from_millis(100));
        anyhow::ensure!(
            outcome.is_none(),
            "elapsed below warn must return None, got {outcome:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_violation_at_warn_returns_warn(_ctx: &Test) -> anyhow::Result<()> {
        let thresholds = EnsureTimes::integration_defaults();
        let outcome = thresholds.violation(Duration::from_millis(500));
        anyhow::ensure!(
            matches!(outcome, Some(EnsureTimeViolation::Warn)),
            "elapsed at warn threshold must return Warn, got {outcome:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_violation_at_critical_returns_critical(_ctx: &Test) -> anyhow::Result<()> {
        let thresholds = EnsureTimes::integration_defaults();
        let outcome = thresholds.violation(Duration::from_secs(1));
        anyhow::ensure!(
            matches!(outcome, Some(EnsureTimeViolation::Critical)),
            "elapsed at critical threshold must return Critical, got {outcome:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_violation_well_above_critical_returns_critical(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let thresholds = EnsureTimes::integration_defaults();
        let outcome = thresholds.violation(Duration::from_mins(1));
        anyhow::ensure!(
            matches!(outcome, Some(EnsureTimeViolation::Critical)),
            "elapsed well above critical must return Critical, got {outcome:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn ensure_time_no_longer_recorded_in_compat_consumed(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--ensure-time", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            !cfg.compat_consumed
                .iter()
                .any(|item| item.starts_with("--ensure-time")),
            "ensure-time has real semantics now; should not be in compat_consumed: {:?}",
            cfg.compat_consumed,
        );
        Ok(())
    }

    #[rudzio::test]
    fn logfile_writer_truncates_existing_file_on_open(_ctx: &Test) -> anyhow::Result<()> {
        let path = temp_dir().join(format!(
            "rudzio-logfile-trunc-{}-{}.log",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
        ));
        fs::write(&path, "stale content from a previous run\n")?;
        let writer = LogfileWriter::open(Some(&path));
        writer.write_line("ok", "fresh::test");
        writer.flush();
        let contents = fs::read_to_string(&path)?;
        let _removed = fs::remove_file(&path);
        anyhow::ensure!(contents == "ok fresh::test\n", "got {contents:?}");
        Ok(())
    }

    #[rudzio::test]
    fn exact_match_defaults_to_false(_ctx: &Test) -> anyhow::Result<()> {
        let cfg =
            Config::from_argv_and_env(&argv(&["my_filter"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(!cfg.exact_match);
        Ok(())
    }

    #[rudzio::test]
    fn exact_flag_sets_exact_match(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--exact", "my_filter"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.exact_match);
        anyhow::ensure!(cfg.filter.as_deref() == Some("my_filter"));
        Ok(())
    }

    #[rudzio::test]
    fn quiet_long_form_selects_terse_format(_ctx: &Test) -> anyhow::Result<()> {
        let cfg =
            Config::from_argv_and_env(&argv(&["--quiet"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(matches!(cfg.format, Format::Terse));
        Ok(())
    }

    #[rudzio::test]
    fn quiet_short_form_selects_terse_format(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&["-q"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(matches!(cfg.format, Format::Terse));
        Ok(())
    }

    #[rudzio::test]
    fn skip_filters_accumulate(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--skip=foo", "--skip", "bar"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.skip_filters == vec!["foo".to_owned(), "bar".to_owned()]);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_defaults_to_threads(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=4"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads == 4);
        anyhow::ensure!(cfg.concurrency_limit == 4);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_is_independent_when_set(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--concurrency-limit=2"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.threads == 8);
        anyhow::ensure!(cfg.concurrency_limit == 2);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_split_form(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--concurrency-limit", "3"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.concurrency_limit == 3);
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_defaults_to_threads(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.parallel_hardlimit.map(NonZeroUsize::get) == Some(8),
            "expected Some(8), got {:?}",
            cfg.parallel_hardlimit
        );
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_equals_form(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit=3"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.map(NonZeroUsize::get) == Some(3));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_split_form(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit", "3"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.map(NonZeroUsize::get) == Some(3));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_none_disables_equals_form(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit=none"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.is_none());
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_none_disables_split_form(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit", "none"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.is_none());
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_threads_keyword(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit=threads"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.map(NonZeroUsize::get) == Some(8));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_zero_falls_back_to_default(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit=0"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.map(NonZeroUsize::get) == Some(8));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_invalid_falls_back_to_default(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit=foo"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.map(NonZeroUsize::get) == Some(8));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_bench_auto_disables_when_unset(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--bench"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.parallel_hardlimit.is_none(),
            "expected None under --bench with no explicit flag, got {:?}",
            cfg.parallel_hardlimit
        );
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_explicit_survives_bench(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&[
                "--test-threads=8",
                "--bench",
                "--threads-parallel-hardlimit=4",
            ]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.map(NonZeroUsize::get) == Some(4));
        Ok(())
    }

    #[rudzio::test]
    fn parallel_hardlimit_explicit_none_survives_non_bench(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-threads=8", "--threads-parallel-hardlimit=none"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.parallel_hardlimit.is_none());
        Ok(())
    }

    #[rudzio::test]
    fn env_is_propagated_into_config(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(Some("4")), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.env.get("RUST_TEST_THREADS").map(String::as_str) == Some("4"));
        Ok(())
    }

    #[rudzio::test]
    fn bench_mode_defaults_to_smoke(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.bench_mode == BenchMode::Smoke);
        Ok(())
    }

    #[rudzio::test]
    fn bench_flag_sets_full_mode(_ctx: &Test) -> anyhow::Result<()> {
        let cfg =
            Config::from_argv_and_env(&argv(&["--bench"]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.bench_mode == BenchMode::Full);
        Ok(())
    }

    #[rudzio::test]
    fn no_bench_flag_sets_skip_mode(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--no-bench"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(cfg.bench_mode == BenchMode::Skip);
        Ok(())
    }

    /// A8. `--suite-setup-timeout=N` populates the new field with N
    /// whole seconds. Both the equals form and the split form work, in
    /// keeping with every other timeout flag.
    #[rudzio::test]
    fn parses_suite_setup_timeout_flag(_ctx: &Test) -> anyhow::Result<()> {
        let equals = Config::from_argv_and_env(
            &argv(&["--suite-setup-timeout=12"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            equals.suite_setup_timeout == Some(Duration::from_secs(12)),
            "equals form: got {:?}",
            equals.suite_setup_timeout
        );
        let split = Config::from_argv_and_env(
            &argv(&["--suite-setup-timeout", "7"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            split.suite_setup_timeout == Some(Duration::from_secs(7)),
            "split form: got {:?}",
            split.suite_setup_timeout
        );
        Ok(())
    }

    /// A9. `--suite-teardown-timeout=N` mirrors A8 for the suite teardown
    /// phase. Same dual-form parsing.
    #[rudzio::test]
    fn parses_suite_teardown_timeout_flag(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--suite-teardown-timeout=4"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.suite_teardown_timeout == Some(Duration::from_secs(4)),
            "got {:?}",
            cfg.suite_teardown_timeout
        );
        Ok(())
    }

    /// A10. `--test-setup-timeout=N` populates the per-test setup
    /// default. The runtime-side resolution lets a per-test attribute
    /// override this; the parser just records the default.
    #[rudzio::test]
    fn parses_test_setup_timeout_flag(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-setup-timeout=3"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.test_setup_timeout == Some(Duration::from_secs(3)),
            "got {:?}",
            cfg.test_setup_timeout
        );
        Ok(())
    }

    /// A11. `--test-teardown-timeout=N` mirrors A10 for the per-test
    /// teardown phase.
    #[rudzio::test]
    fn parses_test_teardown_timeout_flag(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--test-teardown-timeout=9"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.test_teardown_timeout == Some(Duration::from_secs(9)),
            "got {:?}",
            cfg.test_teardown_timeout
        );
        Ok(())
    }

    /// A12. A non-numeric value falls through to `unparsed` (the same
    /// degradation mode the existing `--test-timeout` and `--run-timeout`
    /// follow). The field stays `None`.
    #[rudzio::test]
    fn unrecognised_phase_timeout_value_falls_through_to_unparsed(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--suite-setup-timeout=banana"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.suite_setup_timeout.is_none(),
            "non-numeric must not populate field, got {:?}",
            cfg.suite_setup_timeout
        );
        Ok(())
    }

    /// A13. With no flags, every phase timeout defaults to `None` so
    /// the wrapper degrades to "no per-phase budget" — matching today's
    /// unbounded behaviour.
    #[rudzio::test]
    fn defaults_are_none_for_all_phase_timeouts(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(cfg.suite_setup_timeout.is_none());
        anyhow::ensure!(cfg.suite_teardown_timeout.is_none());
        anyhow::ensure!(cfg.test_setup_timeout.is_none());
        anyhow::ensure!(cfg.test_teardown_timeout.is_none());
        Ok(())
    }

    /// L1.3. `--cancel-grace-period=N` populates the new field with N
    /// whole seconds. Both the equals form and the split form work, in
    /// keeping with every other timeout-shaped flag.
    #[rudzio::test]
    fn parses_cancel_grace_period_flag(_ctx: &Test) -> anyhow::Result<()> {
        let equals = Config::from_argv_and_env(
            &argv(&["--cancel-grace-period=12"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            equals.cancel_grace_period == Some(Duration::from_secs(12)),
            "equals form: got {:?}",
            equals.cancel_grace_period
        );
        let split = Config::from_argv_and_env(
            &argv(&["--cancel-grace-period", "7"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            split.cancel_grace_period == Some(Duration::from_secs(7)),
            "split form: got {:?}",
            split.cancel_grace_period
        );
        Ok(())
    }

    /// L1.3b. `--cancel-grace-period=0` explicitly disables the Layer-1
    /// process-exit watchdog. We map zero to `None` rather than to a
    /// degenerate `Some(0)` so the runner can decide "do nothing" purely
    /// from the option discriminant.
    #[rudzio::test]
    fn cancel_grace_period_zero_disables_watchdog(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--cancel-grace-period=0"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.cancel_grace_period.is_none(),
            "zero must disable, got {:?}",
            cfg.cancel_grace_period
        );
        Ok(())
    }

    /// L1.4. With no `--cancel-grace-period` flag, the field defaults to
    /// 5 seconds. The default is meaningful: the watchdog catches
    /// pathological hangs (sync-blocked tasks ignoring SIGINT) without
    /// the user having to opt in. 5s mirrors what most CI systems already
    /// allow before they SIGKILL the runner themselves.
    #[rudzio::test]
    fn cancel_grace_period_defaults_to_five_seconds(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(
            cfg.cancel_grace_period == Some(Duration::from_secs(5)),
            "default must be Some(5s), got {:?}",
            cfg.cancel_grace_period
        );
        Ok(())
    }

    /// L1.4b. A non-numeric value falls through to `unparsed`, like
    /// every other timeout flag. Field stays at the default (5s).
    #[rudzio::test]
    fn cancel_grace_period_garbage_falls_through_to_default(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--cancel-grace-period=banana"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.cancel_grace_period == Some(Duration::from_secs(5)),
            "garbage value must leave default in place, got {:?}",
            cfg.cancel_grace_period
        );
        Ok(())
    }

    /// L2.5. `--phase-hang-grace=N` populates the new field with N whole
    /// seconds. Mirror of cancel-grace, but for the per-phase Layer-2
    /// grace step (different knob, different semantic).
    #[rudzio::test]
    fn parses_phase_hang_grace_flag(_ctx: &Test) -> anyhow::Result<()> {
        let equals = Config::from_argv_and_env(
            &argv(&["--phase-hang-grace=4"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            equals.phase_hang_grace == Some(Duration::from_secs(4)),
            "equals form: got {:?}",
            equals.phase_hang_grace
        );
        let split = Config::from_argv_and_env(
            &argv(&["--phase-hang-grace", "9"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            split.phase_hang_grace == Some(Duration::from_secs(9)),
            "split form: got {:?}",
            split.phase_hang_grace
        );
        Ok(())
    }

    /// L2.5b. `--phase-hang-grace=0` disables the Layer-2 grace step:
    /// when a phase blows its budget the wrapper returns `TimedOut`
    /// immediately rather than waiting and escalating to `Hung`.
    #[rudzio::test]
    fn phase_hang_grace_zero_disables_layer2(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(
            &argv(&["--phase-hang-grace=0"]),
            env_with(None),
            rudzio::cargo_meta!(),
        );
        anyhow::ensure!(
            cfg.phase_hang_grace.is_none(),
            "zero must disable, got {:?}",
            cfg.phase_hang_grace
        );
        Ok(())
    }

    /// L2.6. With no flag, Layer-2 escalation is OFF (`None`). The
    /// inline `run_phase_with_timeout_and_cancel` wrapper otherwise
    /// changes its behaviour for cooperatively-cancellable phases:
    /// the grace step polls the cancellable past `phase_token.cancel()`,
    /// which can let phase bodies observe the cancel and run cleanup
    /// (including post-cancel printlns) before being dropped — a
    /// behavioural shift from the pre-Layer-2 drop-on-cancel
    /// semantics. Users opt into Layer-2 by passing
    /// `--phase-hang-grace=<secs>`.
    #[rudzio::test]
    fn phase_hang_grace_defaults_to_none(_ctx: &Test) -> anyhow::Result<()> {
        let cfg = Config::from_argv_and_env(&argv(&[]), env_with(None), rudzio::cargo_meta!());
        anyhow::ensure!(
            cfg.phase_hang_grace.is_none(),
            "default must be None (Layer-2 opt-in), got {:?}",
            cfg.phase_hang_grace
        );
        Ok(())
    }

    /// L2.7. `SuiteSummary` exposes a `hung` counter that participates
    /// in `merge`. A run that records 2 hung tests in suite A and 1 in
    /// suite B must surface 3 in the merged summary — same shape as
    /// every other counter on the struct.
    #[rudzio::test]
    fn suite_summary_merge_includes_hung(_ctx: &Test) -> anyhow::Result<()> {
        let summary_a = SuiteSummary::new(SummaryOutcomes::new(0, 0, 2, 0, 0, 0, 0), 0, 2);
        let summary_b = SuiteSummary::new(SummaryOutcomes::new(0, 0, 1, 0, 0, 0, 0), 0, 1);
        let merged = summary_a.merge(summary_b);
        anyhow::ensure!(
            merged.hung == 3,
            "merge must sum hung counters, got {}",
            merged.hung
        );
        Ok(())
    }

    /// L2.7b. `SuiteSummary::zero()` must initialise `hung` to 0 just
    /// like every other counter. The macro-generated dispatch loop
    /// starts every group's accumulator from `zero()` and then calls
    /// `record_outcome` per test, so a non-zero default would silently
    /// corrupt every run's count.
    #[rudzio::test]
    fn suite_summary_zero_initialises_hung_to_zero(_ctx: &Test) -> anyhow::Result<()> {
        let zero = SuiteSummary::zero();
        anyhow::ensure!(zero.hung == 0, "zero() must give hung=0, got {}", zero.hung);
        Ok(())
    }

    /// L2.8. `TestSummary` (the runner-level aggregate) propagates
    /// `hung` from the per-suite summaries and includes it in
    /// `is_success`: a run with any hung tests is NOT a success and
    /// must produce a non-zero exit code, even if every test that DID
    /// run passed.
    ///
    /// `TestSummary` is `#[non_exhaustive]` so we mutate fields
    /// post-`zero()` rather than using struct-expression syntax — same
    /// pattern external users have to follow.
    #[rudzio::test]
    fn test_summary_is_success_false_when_hung_gt_zero(_ctx: &Test) -> anyhow::Result<()> {
        let mut summary = TestSummary::zero();
        summary.passed = 5;
        summary.hung = 1;
        summary.total = 6;
        anyhow::ensure!(
            !summary.is_success(),
            "is_success must return false when hung > 0"
        );
        anyhow::ensure!(
            summary.exit_code() == 1_i32,
            "exit_code must be 1 when hung > 0, got {}",
            summary.exit_code()
        );
        Ok(())
    }

    /// L2.8b. Symmetric check: a run with hung=0 and every other
    /// failure-class counter at 0 is still a success — adding the new
    /// field must not regress the happy path.
    #[rudzio::test]
    fn test_summary_is_success_true_when_all_failure_counts_zero(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let mut summary = TestSummary::zero();
        summary.passed = 10;
        summary.ignored = 2;
        summary.total = 12;
        anyhow::ensure!(
            summary.is_success(),
            "is_success must remain true when no failures and hung=0"
        );
        anyhow::ensure!(
            summary.exit_code() == 0_i32,
            "exit_code must be 0, got {}",
            summary.exit_code()
        );
        Ok(())
    }

    /// L2.8c. `TestSummary::merge` includes `hung`. Two summaries with
    /// hung counts 4 and 2 merge to 6.
    #[rudzio::test]
    fn test_summary_merge_includes_hung(_ctx: &Test) -> anyhow::Result<()> {
        let mut left = TestSummary::zero();
        left.hung = 4;
        left.total = 4;
        let mut right = TestSummary::zero();
        right.hung = 2;
        right.total = 2;
        let merged = left.merge(right);
        anyhow::ensure!(
            merged.hung == 6,
            "TestSummary::merge must sum hung counters, got {}",
            merged.hung
        );
        Ok(())
    }

    #[rudzio::test]
    fn test_summary_is_success_false_when_ensure_time_exceeded_gt_zero(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let mut summary = TestSummary::zero();
        summary.passed = 5;
        summary.ensure_time_exceeded = 1;
        summary.total = 5;
        anyhow::ensure!(
            !summary.is_success(),
            "is_success must return false when ensure_time_exceeded > 0",
        );
        anyhow::ensure!(
            summary.exit_code() == 1_i32,
            "exit_code must be 1 when ensure_time_exceeded > 0, got {}",
            summary.exit_code(),
        );
        Ok(())
    }

    #[rudzio::test]
    fn test_summary_merge_includes_ensure_time_exceeded(_ctx: &Test) -> anyhow::Result<()> {
        let mut left = TestSummary::zero();
        left.ensure_time_exceeded = 3;
        left.total = 3;
        let mut right = TestSummary::zero();
        right.ensure_time_exceeded = 4;
        right.total = 4;
        let merged = left.merge(right);
        anyhow::ensure!(
            merged.ensure_time_exceeded == 7,
            "TestSummary::merge must sum ensure_time_exceeded counters, got {}",
            merged.ensure_time_exceeded,
        );
        Ok(())
    }

    #[rudzio::test]
    fn test_summary_zero_initialises_ensure_time_exceeded_to_zero(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let zero = TestSummary::zero();
        anyhow::ensure!(
            zero.ensure_time_exceeded == 0,
            "zero() must give ensure_time_exceeded=0, got {}",
            zero.ensure_time_exceeded,
        );
        Ok(())
    }
}

/// Strategy-level smoke tests dogfooded across every runtime rudzio
/// ships. The stock strategies only need "poll this future" and
/// `futures::join_all`, so they're independent of the runtime's
/// concurrency model — proving that on every backend is a cheap POC.
#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod bench_strategies {
    use rudzio::bench::Strategy as _;

    use super::{AtomicUsize, BoxError, Concurrent, Ordering, Report, Sequential, Test, box_error};

    #[rudzio::test]
    async fn sequential_runs_body_n_times(_ctx: &Test) -> anyhow::Result<()> {
        let count = AtomicUsize::new(0);
        let report: Report = Sequential::new(7)
            .run(
                || async {
                    let _prev = count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |_| {},
            )
            .await;
        anyhow::ensure!(count.load(Ordering::SeqCst) == 7);
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
        let count = AtomicUsize::new(0);
        let report: Report = Concurrent::new(5)
            .run(
                || async {
                    let _prev = count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |_| {},
            )
            .await;
        anyhow::ensure!(count.load(Ordering::SeqCst) == 5);
        anyhow::ensure!(report.iterations == 5);
        anyhow::ensure!(report.samples.len() == 5);
        anyhow::ensure!(report.is_success());
        anyhow::ensure!(report.strategy == "Concurrent(5)");
        Ok(())
    }

    #[rudzio::test]
    async fn sequential_captures_failures(_ctx: &Test) -> anyhow::Result<()> {
        let counter = AtomicUsize::new(0);
        let report = Sequential::new(4)
            .run(
                || async {
                    let prev = counter.fetch_add(1, Ordering::SeqCst);
                    if prev.is_multiple_of(2) {
                        Ok(())
                    } else {
                        Err(box_error("even iteration required"))
                    }
                },
                |_| {},
            )
            .await;
        anyhow::ensure!(report.samples.len() == 2);
        anyhow::ensure!(report.failures.len() == 2);
        anyhow::ensure!(!report.is_success());
        Ok(())
    }

    #[rudzio::test]
    async fn empty_samples_return_none_for_stats(_ctx: &Test) -> anyhow::Result<()> {
        let report = Sequential::new(0)
            .run(|| async { Ok::<(), BoxError>(()) }, |_| {})
            .await;
        anyhow::ensure!(report.min().is_none());
        anyhow::ensure!(report.max().is_none());
        anyhow::ensure!(report.mean().is_none());
        anyhow::ensure!(report.median().is_none());
        anyhow::ensure!(report.percentile_permille(500_u32).is_none());
        anyhow::ensure!(report.ascii_histogram(8, 20).is_empty());
        Ok(())
    }

    #[rudzio::test]
    async fn percentile_clamps_high_permille(_ctx: &Test) -> anyhow::Result<()> {
        let report = Sequential::new(3)
            .run(|| async { Ok::<(), BoxError>(()) }, |_| {})
            .await;
        // permille saturates at 1000; well-defined for any non-empty run.
        anyhow::ensure!(report.percentile_permille(0_u32).is_some());
        anyhow::ensure!(report.percentile_permille(1000_u32).is_some());
        anyhow::ensure!(report.percentile_permille(2000_u32).is_some());
        Ok(())
    }

    // End-to-end: a bench-annotated test. Under `cargo test` this runs
    // once as a smoke test (body invoked exactly once, no stats
    // collection). Under `cargo test -- --bench` it runs with the
    // strategy. The iteration count stays tiny so the smoke path
    // doesn't dominate the runtime sweep.
    #[rudzio::test(benchmark = Sequential::new(3))]
    async fn sample_sequential_bench(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test(benchmark = Concurrent::new(3))]
    async fn sample_concurrent_bench(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    // Bench tests without a context parameter also work — setup and
    // teardown still run around the strategy invocation.
    #[rudzio::test(benchmark = Sequential::new(2))]
    async fn sample_bench_without_ctx() -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod build_sentinel {
    use super::{
        NESTED_SENTINEL_ENV, OsStr, SentinelAction, decide_sentinel_action,
        sentinel_indicates_nested_call,
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
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod bin_resolver {
    use super::{__resolve_at_runtime, Path, current_exe};

    #[rudzio::test]
    fn runtime_walk_reaches_a_directory_that_exists() -> anyhow::Result<()> {
        let current = current_exe()?;
        let profile_dir = current
            .parent()
            .and_then(Path::parent)
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
        // `expect_err` would panic on `Ok` — and panics are forbidden in
        // src/. Convert the Ok path into a typed failure instead so the
        // assertion is a proper Result-bail, not a panic.
        let Err(err) = __resolve_at_runtime("this-bin-definitely-does-not-exist-xyz-123") else {
            anyhow::bail!("bogus bin name must not resolve");
        };
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

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod filter_matching {
    use super::{RunIgnoredMode, Test, token_passes_filters};

    /// Build a `Vec<String>` skip list from string slices — same shape
    /// as `argv` but local to this module so the suite spec only
    /// imports what each module needs.
    fn skips(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    // Regression: a path-shaped substring copied from runner output must
    // match the test it came from. Before the fix this returned false
    // because the filter looked at the leaf name only.
    #[rudzio::test]
    fn path_shaped_filter_matches_qualified_name(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(token_passes_filters(
            "crate::write_layer::main::e2e_inproc::doput",
            false,
            Some("e2e_inproc"),
            &[],
            RunIgnoredMode::Normal,
            false,
        ));
        Ok(())
    }

    // Existing leaf-name workflow keeps working under the new rule
    // because the leaf is a substring of the qualified name.
    #[rudzio::test]
    fn leaf_name_filter_still_matches(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(token_passes_filters(
            "crate::write_layer::main::e2e_inproc::doput",
            false,
            Some("doput"),
            &[],
            RunIgnoredMode::Normal,
            false,
        ));
        Ok(())
    }

    #[rudzio::test]
    fn non_matching_filter_rejects(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(!token_passes_filters(
            "crate::write_layer::main::e2e_inproc::doput",
            false,
            Some("nope"),
            &[],
            RunIgnoredMode::Normal,
            false,
        ));
        Ok(())
    }

    // Regression: --skip with a path-shaped substring must reject tests
    // in that module. Before the fix, --skip "file_v3::" matched no
    // tests because skip looked at the leaf only.
    #[rudzio::test]
    fn path_shaped_skip_rejects(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(!token_passes_filters(
            "crate::storage::file_v3::reads::ok",
            false,
            None,
            &skips(&["file_v3::"]),
            RunIgnoredMode::Normal,
            false,
        ));
        Ok(())
    }

    #[rudzio::test]
    fn path_shaped_skip_leaves_siblings(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(token_passes_filters(
            "crate::storage::file_v2::reads::ok",
            false,
            None,
            &skips(&["file_v3::"]),
            RunIgnoredMode::Normal,
            false,
        ));
        Ok(())
    }

    #[rudzio::test]
    fn skip_wins_over_filter(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(!token_passes_filters(
            "crate::e2e_inproc::doput",
            false,
            Some("e2e_inproc"),
            &skips(&["doput"]),
            RunIgnoredMode::Normal,
            false,
        ));
        Ok(())
    }

    #[rudzio::test]
    fn run_ignored_only_rejects_non_ignored(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(!token_passes_filters(
            "crate::any::test",
            false,
            None,
            &[],
            RunIgnoredMode::Only,
            false,
        ));
        Ok(())
    }

    #[rudzio::test]
    fn run_ignored_only_keeps_ignored(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(token_passes_filters(
            "crate::any::test",
            true,
            None,
            &[],
            RunIgnoredMode::Only,
            false,
        ));
        Ok(())
    }

    // --exact (libtest compat): filter must equal the qualified name,
    // not merely be a substring of it.
    #[rudzio::test]
    fn exact_filter_rejects_substring(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(!token_passes_filters(
            "crate::foo::doput",
            false,
            Some("doput"),
            &[],
            RunIgnoredMode::Normal,
            true,
        ));
        Ok(())
    }

    #[rudzio::test]
    fn exact_filter_accepts_full_match(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(token_passes_filters(
            "crate::foo::doput",
            false,
            Some("crate::foo::doput"),
            &[],
            RunIgnoredMode::Normal,
            true,
        ));
        Ok(())
    }

    // Skip is sibling-symmetric with filter under --exact: only an
    // entire-name match excludes a test, partial-name skip values
    // become no-ops.
    #[rudzio::test]
    fn exact_skip_rejects_only_full_match(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(!token_passes_filters(
            "crate::foo::doput",
            false,
            None,
            &skips(&["crate::foo::doput"]),
            RunIgnoredMode::Normal,
            true,
        ));
        Ok(())
    }

    #[rudzio::test]
    fn exact_skip_ignores_substring(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(token_passes_filters(
            "crate::foo::doput",
            false,
            None,
            &skips(&["doput"]),
            RunIgnoredMode::Normal,
            true,
        ));
        Ok(())
    }
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod path_normalize {
    use super::{Test, normalize_module_path, qualified_test_name};

    // Per-crate mode: the cargo `[[test]] name = "main"` test binary
    // makes the leading segment `main`. Drop it so the displayed path
    // begins at the user's first real module.
    #[rudzio::test]
    fn per_crate_main_prefix_is_stripped(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(normalize_module_path("main::runner::test_case") == "runner::test_case");
        Ok(())
    }

    // Aggregator mode: `<aggregator>::tests::<crate>::main::<rest>` —
    // drop the aggregator crate, the `tests` wrapper, and the inner
    // `main` shim, leaving the user's crate at the head.
    #[rudzio::test]
    fn aggregator_prefix_is_stripped(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(
            normalize_module_path("rudzio_auto_runner::tests::write_layer::main::e2e::doput")
                == "write_layer::e2e::doput"
        );
        Ok(())
    }

    #[rudzio::test]
    fn tests_wrapper_only_dropped_after_crate(_ctx: &Test) -> anyhow::Result<()> {
        // A `tests` segment further down the path stays intact —
        // only the aggregator's immediate `tests::` wrapper is
        // autogenerated.
        anyhow::ensure!(
            normalize_module_path("crate::user_mod::tests::case") == "user_mod::tests::case"
        );
        Ok(())
    }

    #[rudzio::test]
    fn inner_main_segments_are_stripped(_ctx: &Test) -> anyhow::Result<()> {
        // An interior `main` segment (the aggregator's `tests/main.rs`
        // shim mounted as `mod main`) is rudzio convention, not user
        // code, so drop it.
        anyhow::ensure!(
            normalize_module_path("agg::tests::crate_x::main::body::leaf") == "crate_x::body::leaf"
        );
        Ok(())
    }

    #[rudzio::test]
    fn empty_after_strip_returns_empty(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(normalize_module_path("main") == "");
        anyhow::ensure!(normalize_module_path("") == "");
        Ok(())
    }

    #[rudzio::test]
    fn qualified_name_joins_normalized_path(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(
            qualified_test_name("main::runner::sub", "test_case") == "runner::sub::test_case"
        );
        Ok(())
    }

    #[rudzio::test]
    fn qualified_name_omits_separator_when_path_empty(_ctx: &Test) -> anyhow::Result<()> {
        // No leading `::` when normalization produces nothing.
        anyhow::ensure!(qualified_test_name("main", "test_case") == "test_case");
        Ok(())
    }

    #[rudzio::test]
    fn bridge_build_rs_overrides_cargo_manifest_dir_for_env_macro(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        // Pin that the bridge `build.rs` directive
        // `cargo:rustc-env=CARGO_MANIFEST_DIR=<member dir>` reaches
        // the `env!()` channel — cargo could silently strip overrides
        // of reserved env vars there. The const captures
        // `env!("CARGO_MANIFEST_DIR")` at rudzio lib's compile site;
        // under the aggregator that compile runs through the bridge.
        let captured = ::rudzio::__BRIDGE_OBSERVED_MANIFEST_DIR;
        anyhow::ensure!(
            !captured.contains("rudzio-auto-runner"),
            "bridge CARGO_MANIFEST_DIR override is being stripped from `env!()`: \
             rudzio's compile saw `{captured}` (the bridge dir under the \
             aggregator's target), not the member's original manifest dir."
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_build_rs_overrides_cargo_manifest_dir_for_proc_macros(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        // Same redirect, different observation channel: `std::env::var`
        // at proc-macro expansion time. Third-party proc-macros like
        // `refinery::embed_migrations!` and `sqlx::migrate!` resolve
        // their path arguments through `std::env::var`. Cargo may pass
        // build-script env overrides to rustc via a private channel
        // that only `env!` reads — this test catches that case so we
        // know whether the redirect actually closes the proc-macro
        // path-resolution gap or only the compile-time `env!` one.
        let captured = ::rudzio::__BRIDGE_PROC_MACRO_OBSERVED_MANIFEST_DIR;
        anyhow::ensure!(
            !captured.contains("rudzio-auto-runner"),
            "bridge CARGO_MANIFEST_DIR override is not reaching proc-macros: \
             `std::env::var(\"CARGO_MANIFEST_DIR\")` at rudzio lib's compile \
             site saw `{captured}` (the bridge dir under the aggregator's \
             target), so any path-resolving proc-macro in member src \
             (`refinery::embed_migrations!`, `sqlx::migrate!`, askama, …) \
             still looks in the wrong place. The bridge directory needs a \
             different rerouting strategy (full-mirror symlink tree, or \
             different env-passing approach)."
        );
        Ok(())
    }
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod runtime_ctx_api {
    use std::time::{Duration, Instant};

    use rudzio::context::Test as _;

    use super::Test;

    /// `Test::sleep` delegates to the suite's runtime, so test bodies can
    /// sleep runtime-agnostically — no `tokio::time::sleep` or other
    /// adapter-specific timer is needed. Asserted across all six adapters
    /// so the contract holds wherever the suite is dispatched.
    #[rudzio::test]
    async fn ctx_sleep_completes_after_requested_duration(ctx: &Test) -> anyhow::Result<()> {
        let start = Instant::now();
        ctx.sleep(Duration::from_millis(15_u64)).await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            elapsed >= Duration::from_millis(10_u64),
            "ctx.sleep must wait at least roughly the requested duration, elapsed={elapsed:?}"
        );
        Ok(())
    }

    /// `Test::name` surfaces `Runtime::name` so test bodies can branch on
    /// the active adapter without coupling to a specific runtime type.
    /// Asserted across all six adapters: each must return a non-empty,
    /// stable identifier.
    #[rudzio::test]
    async fn ctx_name_returns_nonempty_runtime_identifier(ctx: &Test) -> anyhow::Result<()> {
        let name = ctx.name();
        anyhow::ensure!(
            !name.is_empty(),
            "ctx.name must return a non-empty runtime identifier"
        );
        Ok(())
    }
}
