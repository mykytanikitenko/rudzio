//! Exercises running non-async work on the runtime's blocking pool via
//! `Test::spawn_blocking`. Proves that:
//!
//! 1. A pure synchronous function awaited through `spawn_blocking` runs
//!    to completion and yields the correct value.
//! 2. The sync work lands on a different OS thread than the async test
//!    body — i.e. it really goes to the blocking pool instead of
//!    monopolising the worker that's polling the task.

use std::thread;
use std::thread::ThreadId;

use common_context::Test;
use rudzio::runtime::tokio::Multithread;

/// Deterministic CPU-bound work. Expected sum: 500500.
fn triangular_sum(n: u64) -> u64 {
    (1..=n).sum()
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::{Test, ThreadId, thread, triangular_sum};

    #[rudzio::test]
    async fn runs_sync_fn_via_spawn_blocking(ctx: &Test) -> anyhow::Result<()> {
        let sum = ctx
            .spawn_blocking(|| triangular_sum(1000))
            .await
            .map_err(|err| anyhow::anyhow!("spawn_blocking failed: {err}"))?;
        anyhow::ensure!(sum == 500_500, "unexpected sum: {sum}");
        Ok(())
    }

    #[rudzio::test]
    async fn spawn_blocking_uses_a_different_thread(ctx: &Test) -> anyhow::Result<()> {
        let async_thread: ThreadId = thread::current().id();
        let blocking_thread: ThreadId = ctx
            .spawn_blocking(|| thread::current().id())
            .await
            .map_err(|err| anyhow::anyhow!("spawn_blocking failed: {err}"))?;
        anyhow::ensure!(
            async_thread != blocking_thread,
            "expected spawn_blocking to offload to a different thread; \
             both ran on {async_thread:?}",
        );
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
