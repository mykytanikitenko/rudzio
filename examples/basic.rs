//! Minimal rudzio example: one runtime, the ready-made `common`
//! context, a handful of tests. Run with:
//!
//! ```sh
//! cargo run --example basic
//! ```

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn sync_pass(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    async fn yields_then_passes(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }

    #[rudzio::test]
    #[ignore = "demonstrates #[ignore] — not run without --include-ignored"]
    async fn skipped_by_default(_ctx: &Test) -> anyhow::Result<()> {
        unreachable!("ignored tests don't execute unless `--include-ignored` is passed")
    }
}

#[rudzio::main]
fn main() {}
