//! Pin tests for the `spawn_env` recursion-guard contract.

use cargo_rudzio::{EXPOSE_BINS_SENTINEL_ENV, spawn_env};
use rudzio::common::context::Suite;
use rudzio::runtime::async_std::Runtime as AsyncStdRuntime;
use rudzio::runtime::compio::Runtime as CompioRuntime;
use rudzio::runtime::embassy::Runtime as EmbassyRuntime;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;
use rudzio::runtime::tokio::{
    CurrentThread as TokioCurrentThread, Local as TokioLocal, Multithread as TokioMultithread,
};

#[rudzio::suite([
    (
        runtime = TokioMultithread::new,
        suite = Suite,
        test = Test,
    ),
    (
        runtime = TokioCurrentThread::new,
        suite = Suite,
        test = Test,
    ),
    (
        runtime = TokioLocal::new,
        suite = Suite,
        test = Test,
    ),
    (
        runtime = CompioRuntime::new,
        suite = Suite,
        test = Test,
    ),
    (
        runtime = EmbassyRuntime::new,
        suite = Suite,
        test = Test,
    ),
    (
        runtime = FuturesThreadPool::new,
        suite = Suite,
        test = Test,
    ),
    (
        runtime = AsyncStdRuntime::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::{EXPOSE_BINS_SENTINEL_ENV, spawn_env};

    #[rudzio::test]
    fn spawn_env_sets_expose_bins_sentinel() -> anyhow::Result<()> {
        // Pin contract: spawn_env sets the recursion-guard sentinel for
        // `rudzio::build::expose_bins`. Without this, a member's build.rs
        // calling `expose_self_bins()` under the aggregator chain would
        // recurse into another `cargo build --bins` and accumulate
        // nested target dirs.
        let env = spawn_env();
        let sentinel = env
            .iter()
            .find(|(key, _)| *key == EXPOSE_BINS_SENTINEL_ENV)
            .map(|(_, value)| value.as_str());
        anyhow::ensure!(
            sentinel == Some("1"),
            "spawn_env must set the expose-bins re-entry sentinel to \"1\", got {sentinel:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn expose_bins_sentinel_name_matches_rudzio_build_module() -> anyhow::Result<()> {
        // Contract: this name MUST match the const `NESTED_SENTINEL_ENV`
        // in `rudzio/src/build.rs`. If either changes without the other,
        // the sentinel stops working and bridge-forwarded build.rs calls
        // to expose_self_bins regress to "no [[bin]]" errors.
        anyhow::ensure!(
            EXPOSE_BINS_SENTINEL_ENV == "__RUDZIO_EXPOSE_BINS_ACTIVE",
            "sentinel env-var name drift detected: {EXPOSE_BINS_SENTINEL_ENV}"
        );
        Ok(())
    }
}
