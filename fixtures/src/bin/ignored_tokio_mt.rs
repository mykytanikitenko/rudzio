use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture asserts #[ignore]'d tests never run; the ignored bodies diverge so their anyhow::Result<()> wrappers are statically unreachable, and the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn runs(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[ignore = "this fixture asserts the runner skips #[ignore]'d tests; the body would panic if it ever ran"]
    #[expect(
        clippy::panic,
        reason = "this fixture asserts #[ignore]'d tests never execute; the body panics so any regression that runs the ignored test is loudly observable"
    )]
    fn ignored_bare(_ctx: &Test) -> anyhow::Result<()> {
        panic!("must not run")
    }

    #[rudzio::test]
    #[ignore = "takes too long"]
    #[expect(
        clippy::panic,
        reason = "this fixture asserts #[ignore]'d tests never execute; the body panics so any regression that runs the ignored test is loudly observable"
    )]
    fn ignored_with_reason(_ctx: &Test) -> anyhow::Result<()> {
        panic!("must not run")
    }
}

#[rudzio::main]
fn main() {}
