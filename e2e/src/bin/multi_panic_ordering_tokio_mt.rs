//! Interleaves panicking and passing tests to verify:
//!   - panic isolation works for every panic, not just the first;
//!   - tests within a runtime group run sequentially in source order
//!     (the macro expands to `#(#test_executions)*` awaited one at a time);
//!   - the final summary counts across multiple panics are correct.

// Test bodies panic on purpose to exercise rudzio's panic isolation.
#![allow(
    clippy::panic,
    reason = "test fixture intentionally panics to exercise the framework"
)]

use common_context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn step_1_pass(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn step_2_panic(_ctx: &Test) -> anyhow::Result<()> {
        panic!("first planned panic");
    }

    #[rudzio::test]
    fn step_3_pass(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn step_4_panic(_ctx: &Test) -> anyhow::Result<()> {
        panic!("second planned panic");
    }

    #[rudzio::test]
    fn step_5_pass(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
