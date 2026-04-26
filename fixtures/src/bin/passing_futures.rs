use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::futures::ThreadPool;

#[rudzio::suite([
    (
        runtime = ThreadPool::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn passes_under_futures(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }

    #[rudzio::test]
    async fn spawn_works_under_futures(ctx: &Test) -> anyhow::Result<()> {
        let result = ctx
            .spawn(async { 7_u32 })
            .await
            .map_err(|err| anyhow::anyhow!("spawn failed: {err}"))?;
        anyhow::ensure!(result == 7_u32, "spawn returned wrong value: {result}");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
