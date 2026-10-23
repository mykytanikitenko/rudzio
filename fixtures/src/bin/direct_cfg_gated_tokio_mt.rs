//! Direct `#[rudzio::test]` fns carrying their own `#[cfg(...)]`.
//!
//! The cfg-false fn is stripped by the compiler; its generated registration
//! items (the `__rudzio_run_test_*` helper + `__RUDZIO_TOKEN_*` static) must
//! carry the same `#[cfg]` so they vanish with it instead of leaving dangling
//! references to the stripped fn (E0425). The cfg-true and ungated fns remain
//! registered and run.

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

    #[cfg(feature = "never-enabled")]
    #[rudzio::test]
    async fn cfg_false_test(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[cfg(not(feature = "never-enabled"))]
    #[rudzio::test]
    async fn cfg_true_test(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    async fn ungated_test(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
