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
        reason = "this fixture exercises the basic happy-path: two tests that simply pass; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn first_passes(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture exercises the basic happy-path: two tests that simply pass; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn second_passes(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
