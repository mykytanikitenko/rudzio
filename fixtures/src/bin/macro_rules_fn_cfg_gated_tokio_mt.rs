//! A `macro_rules!`-generated test fn carrying its own `#[cfg(...)]`.
//!
//! The `make_gated_test!` macro emits `#[cfg(feature = "never-enabled")]`
//! directly on the generated fn (unlike `macro_rules_cfg_gated_tokio_mt.rs`,
//! which gates the *invocation*). The `#[rudzio::test]` attribute forwards the
//! fn to `__register_test!` with the `#[cfg]` intact, so the per-test
//! registration items must carry it too (else E0425 when the fn is stripped).

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

/// Generate a trivially-passing `#[rudzio::test]` fn gated by `#[cfg]`.
macro_rules! make_gated_test {
    ($name:ident) => {
        #[cfg(feature = "never-enabled")]
        #[rudzio::test]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// Generate a trivially-passing, ungated `#[rudzio::test]` fn.
macro_rules! make_plain_test {
    ($name:ident) => {
        #[rudzio::test]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    make_plain_test!(always_on);
    make_gated_test!(cfg_gated_off);
}

#[rudzio::main]
fn main() {}
