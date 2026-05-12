use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture pairs a passing test with a deliberately failing one to assert the runner reports a mixed-outcome summary; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn passes(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn fails(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::bail!("intentional failure")
    }
}

#[rudzio::main]
fn main() {}
