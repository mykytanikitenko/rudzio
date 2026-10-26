//! A `#[rudzio::suite]` mod nested inside another `#[rudzio::suite]` mod.
//!
//! Each suite is an independent attribute macro on its own `mod`, so the
//! outer suite must pass the inner (still-attributed) mod through its body
//! untouched and register only its own direct tests; the inner suite then
//! expands and registers its own. Both sets must land in the distributed
//! slice and run under a single `#[rudzio::main]`.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod outer {
    use super::{Multithread, Suite, Test};

    #[rudzio::test]
    async fn outer_direct(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::suite([
        (
            runtime = Multithread::new,
            suite = Suite,
            test = Test,
        ),
    ])]
    mod inner {
        use super::Test;

        #[rudzio::test]
        async fn inner_direct(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    }
}

#[rudzio::main]
fn main() {}
