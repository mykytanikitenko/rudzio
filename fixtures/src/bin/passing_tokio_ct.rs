use rudzio::common::context::Test;
use rudzio::runtime::tokio::CurrentThread;

#[rudzio::suite([
    (
        runtime = CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use rudzio::context::Test as _;

    use super::Test;

    #[rudzio::test]
    async fn yields_then_passes(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
