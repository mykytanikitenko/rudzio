//! Run the same tests under three async runtimes in a single
//! `#[rudzio::suite]` block. Each runtime gets its own OS thread, its
//! own suite-level context, and its own per-test context — the runner
//! dispatches tests across runtime groups concurrently.
//!
//! ```sh
//! cargo run --example multi_runtime
//! ```

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::compio;
use rudzio::runtime::tokio::{CurrentThread, Multithread};

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use rudzio::context::Test as _;

    use super::Test;

    #[rudzio::test]
    async fn same_body_every_runtime(ctx: &Test) -> anyhow::Result<()> {
        // `yield_now` works on every runtime rudzio ships — useful as a
        // cheap portability check when you're exercising a code path
        // that hits the scheduler.
        ctx.yield_now().await;
        Ok(())
    }

    #[rudzio::test]
    async fn spawn_works(ctx: &Test) -> anyhow::Result<()> {
        let value = ctx
            .spawn(async { 42_u32 })
            .await
            .map_err(|err| anyhow::anyhow!("spawn failed: {err}"))?;
        anyhow::ensure!(value == 42);
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
