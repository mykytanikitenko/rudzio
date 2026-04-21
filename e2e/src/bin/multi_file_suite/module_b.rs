//! Module B: contributes a separate suite.
//!
//! Demonstrates that tests spread across multiple files collapse into one
//! run under a single `rudzio::run()` call.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
pub mod tests_b {
    use super::Test;

    #[rudzio::test]
    fn module_b_first(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn module_b_second(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}
