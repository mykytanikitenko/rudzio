use common_context::Test;
use rudzio::runtime::tokio::CurrentThread;

#[rudzio::suite([
    (
        runtime = CurrentThread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn yields_then_passes(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
