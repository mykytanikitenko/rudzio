//! Minimal rudzio example: one runtime, the ready-made `common`
//! context, a handful of tests. Run with:
//!
//! ```sh
//! cargo run --example basic
//! ```

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn sync_pass(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(1_i32 + 1_i32 == 2_i32);
        Ok(())
    }

    #[rudzio::test]
    async fn yields_then_passes(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }

    // The context parameter is optional. Suite setup and per-test
    // teardown still run — the test body just doesn't see the context.
    #[rudzio::test]
    fn pass_without_context() -> anyhow::Result<()> {
        anyhow::ensure!(2_i32 + 2_i32 == 4_i32);
        Ok(())
    }

    #[rudzio::test]
    #[ignore = "demonstrates #[ignore] — not run without --include-ignored"]
    async fn skipped_by_default(_ctx: &Test) -> anyhow::Result<()> {
        // `unreachable!` would be a forbidden panic-path. `bail!` just
        // returns Err — same observable effect (the test would fail if
        // run), but no panic is involved.
        anyhow::bail!("ignored tests don't execute unless `--include-ignored` is passed")
    }
}

#[rudzio::main]
fn main() {}
