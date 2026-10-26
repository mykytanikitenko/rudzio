//! A `#[cfg(feature = "...")]`-gated suite *mod* — the whole `mod tests`
//! block is stripped when the feature is off, not just a test fn inside it.
//!
//! cfg-stripping runs before attribute-macro expansion, so when the feature
//! is off the gated suite vanishes entirely (its tests must not register);
//! when on it survives and registers. The ungated suite keeps the crate valid
//! in both states. The integration assertion branches on
//! `cfg!(feature = ...)` at runtime, mirroring `macro_rules_cfg_gated_tokio_mt`.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod ungated {
    use super::Test;

    #[rudzio::test]
    async fn always_on(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "edge-cfg-macro")]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod gated {
    use super::Test;

    #[rudzio::test]
    async fn cfg_gated_suite_test(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
