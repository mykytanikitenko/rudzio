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
    fn before_panic(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn panics(_ctx: &Test) -> anyhow::Result<()> {
        panic!("intentional panic to exercise the isolation bug");
    }

    #[rudzio::test]
    fn after_panic(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
