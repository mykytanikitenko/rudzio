//! Module A: contributes its own suite to the distributed test slice.
//!
//! Included from the `multi_file_suite` binary via `#[path]` so tokens defined
//! here register into `rudzio::TEST_TOKENS` alongside tokens from sibling
//! modules. A single `rudzio::run()` call in the binary drives them all.

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
pub mod tests_a {
    use super::Test;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture exercises distributed-slice token registration across multiple files; tests simply pass and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn module_a_first(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture exercises distributed-slice token registration across multiple files; tests simply pass and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn module_a_second(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}
