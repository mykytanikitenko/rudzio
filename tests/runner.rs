//! Rudzio's own dogfood test suite.
//!
//! Lives as a regular integration test (so per-crate `cargo test -p rudzio`
//! just works) AND is re-compiled into the workspace-wide `test-runner/`
//! binary via `#[path]`. That dual-role layout keeps Cargo's workspace
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
    use super::{argv, env_with, Config};
    use rudzio::common::context::Test;

    #[rudzio::test]
    fn joined_argv_form_is_parsed(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["--test-threads=4"]), env_with(None));
        anyhow::ensure!(c.threads == 4, "threads = {}", c.threads);
        Ok(())
    }

    #[rudzio::test]
    fn split_argv_form_is_parsed(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["--test-threads", "8"]), env_with(None));
        anyhow::ensure!(c.threads == 8);
        Ok(())
    }

    #[rudzio::test]
    fn env_var_alone_is_used(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&[]), env_with(Some("3")));
        anyhow::ensure!(c.threads == 3);
        Ok(())
    }

    #[rudzio::test]
    fn argv_takes_precedence_over_env(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["--test-threads=2"]), env_with(Some("7")));
        anyhow::ensure!(c.threads == 2);
        Ok(())
    }

    #[rudzio::test]
    fn zero_threads_falls_through_to_available_parallelism(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["--test-threads=0"]), env_with(Some("0")));
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn garbage_threads_falls_through(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["--test-threads=abc"]), env_with(Some("xyz")));
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn zero_in_env_is_ignored_when_argv_is_valid(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["--test-threads=5"]), env_with(Some("0")));
        anyhow::ensure!(c.threads == 5);
        Ok(())
    }

    #[rudzio::test]
    fn unknown_flags_are_preserved_in_unparsed(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--nocapture", "--color=always", "--test-threads=3", "--format=json"]),
            env_with(None),
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
        let c = Config::from_argv_and_env(argv(&["--test-threads"]), env_with(None));
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn both_unset_uses_available_parallelism(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&[]), env_with(None));
        anyhow::ensure!(c.threads >= 1);
        Ok(())
    }

    #[rudzio::test]
    fn filter_substring_is_captured(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["my_filter"]), env_with(None));
        anyhow::ensure!(c.filter.as_deref() == Some("my_filter"));
        Ok(())
    }

    #[rudzio::test]
    fn skip_filters_accumulate(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--skip=foo", "--skip", "bar"]),
            env_with(None),
        );
        anyhow::ensure!(c.skip_filters == vec!["foo".to_owned(), "bar".to_owned()]);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_defaults_to_threads(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(argv(&["--test-threads=4"]), env_with(None));
        anyhow::ensure!(c.threads == 4);
        anyhow::ensure!(c.concurrency_limit == 4);
        Ok(())
    }

    #[rudzio::test]
    fn concurrency_limit_is_independent_when_set(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&["--test-threads=8", "--concurrency-limit=2"]),
            env_with(None),
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
        );
        anyhow::ensure!(c.concurrency_limit == 3);
        Ok(())
    }

    #[rudzio::test]
    fn env_is_propagated_into_config(_ctx: &Test) -> anyhow::Result<()> {
        let c = Config::from_argv_and_env(
            argv(&[]),
            env_with(Some("4")),
        );
        anyhow::ensure!(c.env.get("RUST_TEST_THREADS").map(String::as_str) == Some("4"));
        Ok(())
    }
}
