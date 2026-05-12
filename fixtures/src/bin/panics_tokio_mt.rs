//! Reproduces the panic-in-test bug.
//!
//! Expected behavior (how `#[test]` / other test harnesses handle this):
//!   - `before_panic` runs and passes
//!   - `panics` panics and is recorded as 1 panicked test
//!   - `after_panic` still runs and passes
//!   - summary: passed=2, panicked=1, total=3
//!
//! Actual behavior today:
//!   The macro imports `FutureExt` but never calls `catch_unwind`, so the
//!   panic unwinds the entire runtime thread. `after_panic` never executes
//!   and the partial summary computed inside `rt.block_on` is lost.

use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture asserts panic-in-test isolation by sandwiching a panicking test between two passing ones; the surrounding pass tests must succeed without doing anything else, and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn before_panic(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::panic,
        reason = "this fixture deliberately triggers a panic to verify the runner isolates it (records 1 panicked test) and continues running subsequent tests; panicking is the test scenario being exercised"
    )]
    fn panics(_ctx: &Test) -> anyhow::Result<()> {
        panic!("intentional panic to exercise the isolation bug");
    }

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture asserts panic-in-test isolation by sandwiching a panicking test between two passing ones; the surrounding pass tests must succeed without doing anything else, and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn after_panic(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
