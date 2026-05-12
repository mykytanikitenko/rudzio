//! Demonstrates the `#[rudzio::test(benchmark = ...)]` annotation.
//!
//! Without `--bench`, each annotated test runs its body exactly once (as
//! a regular test) — the attribute is a no-op. With `--bench`, the runner
//! dispatches through the strategy and prints a per-iteration
//! distribution.
//!
//! Run as a regular test (body invoked once per annotated test):
//!
//! ```sh
//! cargo run --example benchmark
//! ```
//!
//! Run as a benchmark (full strategy execution):
//!
//! ```sh
//! cargo run --example benchmark -- --bench
//! ```
//!
//! Skip bench-annotated tests entirely:
//!
//! ```sh
//! cargo run --example benchmark -- --no-bench
//! ```

use std::time::Duration;

use rudzio::bench::strategy::{Concurrent, Sequential};
use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
mod tests {
    use rudzio::context::Test as _;

    use super::{Duration, Test};

    // Plain test — no bench annotation. Runs once regardless of
    // `--bench`.
    #[rudzio::test]
    async fn plain_test(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(1_i32 + 1_i32 == 2_i32);
        Ok(())
    }

    // Bench-annotated test, sequential strategy. With `--bench` the body
    // runs 64 times one after another; without it, once. `Sequential`
    // resolves via the file-root `use rudzio::bench::strategy::...`
    // import — attribute-argument tokens keep the source span of the
    // attribute site, so the identifier must be in scope at that site.
    #[rudzio::test(benchmark = Sequential::new(64))]
    async fn sequential_bench(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }

    // Concurrent strategy — 16 copies of the body driven concurrently
    // via `futures::join_all`. Useful for latency-under-load shapes.
    #[rudzio::test(benchmark = Concurrent::new(16))]
    async fn concurrent_bench(_ctx: &Test) -> anyhow::Result<()> {
        let _d = Duration::from_nanos(1);
        Ok(())
    }

    // Bench tests don't need a context parameter — setup and teardown
    // still run around the strategy invocation.
    #[rudzio::test(benchmark = Sequential::new(8))]
    async fn bench_without_context() -> anyhow::Result<()> {
        anyhow::ensure!(true);
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
