//! Module B: contributes a separate suite.
//!
//! Demonstrates that tests spread across multiple files collapse into one
//! run under a single `rudzio::run()` call.

use common_context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
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
