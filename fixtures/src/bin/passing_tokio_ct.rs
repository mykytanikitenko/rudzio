use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
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
}

#[rudzio::main]
fn main() {}
