//! A `#[cfg_attr(cond, ignore)]` on a test fn must be respected when `cond`
//! is true. `all()` is vacuously true and `any()` is vacuously false, so this
//! fixture deterministically exercises both the ignored and the run path
//! without a toggled feature.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    // `all()` is always true → this test is always ignored.
    #[cfg_attr(all(), ignore = "always ignored via cfg_attr(all())")]
    #[rudzio::test]
    async fn cfg_attr_ignored(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    // `any()` is always false → the `ignore` never applies, so the test runs.
    #[cfg_attr(any(), ignore)]
    #[rudzio::test]
    async fn cfg_attr_runs(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
