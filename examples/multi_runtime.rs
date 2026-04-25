//! Run the same tests under three async runtimes in a single
//! `#[rudzio::suite]` block. Each runtime gets its own OS thread, its
//! own suite-level context, and its own per-test context — the runner
//! dispatches tests across runtime groups concurrently.
//!
//! ```sh
//! cargo run --example multi_runtime
//! ```

use rudzio::common::context::Test;

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
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
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
