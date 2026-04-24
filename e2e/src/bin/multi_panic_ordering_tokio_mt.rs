//! Interleaves panicking and passing tests to verify:
//!   - panic isolation works for every panic, not just the first;
//!   - tests within a runtime group run sequentially in source order
//!     (the macro expands to `#(#test_executions)*` awaited one at a time);
//!   - the final summary counts across multiple panics are correct.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
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
