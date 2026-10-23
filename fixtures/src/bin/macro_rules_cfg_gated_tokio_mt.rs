//! Bug #5 edge case: a `macro_rules!`-generated test inside a
//! `#[cfg(feature = "...")]`-gated invocation. cfg-stripping runs before the
//! suite attribute macro, so when the feature is OFF the invocation vanishes
//! and its test must NOT be registered; when ON the invocation is present and
//! the test registers. The unconditional `always_on` invocation keeps the
//! module non-empty in both states (and is itself the same-shape control).

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

/// Generate a trivially-passing `#[rudzio::test]` fn.
macro_rules! make_test {
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

    make_test!(always_on);

    #[cfg(feature = "edge-cfg-macro")]
    make_test!(cfg_gated_on);
}

#[rudzio::main]
fn main() {}
