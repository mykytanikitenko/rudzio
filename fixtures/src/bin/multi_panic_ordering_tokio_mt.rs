//! Interleaves panicking and passing tests to verify:
//!   - panic isolation works for every panic, not just the first;
//!   - tests within a runtime group run sequentially in source order
//!     (the macro expands to `#(#test_executions)*` awaited one at a time);
//!   - the final summary counts across multiple panics are correct.

use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture interleaves panicking and passing tests; the panicking bodies diverge so their anyhow::Result<()> wrappers are statically unreachable, and the framework requires the test fn signature to return anyhow::Result<()>"
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
    fn step_1_pass(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::panic,
        reason = "this fixture interleaves panicking and passing tests to verify panic isolation works for every panic, not just the first; the body must panic to exercise that path"
    )]
    fn step_2_panic(_ctx: &Test) -> anyhow::Result<()> {
        panic!("first planned panic");
    }

    #[rudzio::test]
    fn step_3_pass(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::panic,
        reason = "this fixture interleaves panicking and passing tests to verify panic isolation works for every panic, not just the first; the body must panic to exercise that path"
    )]
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
