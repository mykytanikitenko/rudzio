use rudzio::common::context::Test;
use rudzio::runtime::tokio::Local;

#[rudzio::suite([
    (
        runtime = Local::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn yields_then_passes(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }

    #[rudzio::test]
    async fn spawn_local_works(ctx: &Test) -> anyhow::Result<()> {
        let result = ctx.spawn_local(async { 42_u32 }).await?;
        anyhow::ensure!(result == 42, "spawn_local returned wrong value");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
