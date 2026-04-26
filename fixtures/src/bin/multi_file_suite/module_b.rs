//! Module B: contributes a separate suite.
//!
//! Demonstrates that tests spread across multiple files collapse into one
//! run under a single `rudzio::run()` call.

use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
pub mod tests_b {
    use super::Test;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture exercises distributed-slice token registration across multiple files; tests simply pass and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn module_b_first(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture exercises distributed-slice token registration across multiple files; tests simply pass and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn module_b_second(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}
