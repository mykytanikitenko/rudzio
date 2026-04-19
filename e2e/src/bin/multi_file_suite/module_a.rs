//! Module A: contributes its own suite to the distributed test slice.
//!
//! Included from the `multi_file_suite` binary via `#[path]` so tokens defined
//! here register into `rudzio::TEST_TOKENS` alongside tokens from sibling
//! modules. A single `rudzio::run()` call in the binary drives them all.

use common_context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
pub mod tests_a {
    use super::Test;

    #[rudzio::test]
    fn module_a_first(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn module_a_second(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}
