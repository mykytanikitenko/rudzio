use common_context::Test;
use rudzio::runtime::embassy::Runtime as EmbassyRuntime;

#[rudzio::suite([
    (
        runtime = EmbassyRuntime::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn passes_under_embassy(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }

    #[rudzio::test]
    async fn spawn_completes_under_embassy(ctx: &Test) -> anyhow::Result<()> {
        let result = ctx.spawn(async { 99_u32 }).await?;
        anyhow::ensure!(result == 99, "spawned task returned wrong value");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
