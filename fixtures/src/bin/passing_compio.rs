use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::compio::Runtime as CompioRuntime;

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture asserts a passing test compiles and runs under the compio runtime; the body trivially succeeds, and the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = CompioRuntime::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn passes_under_compio(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
