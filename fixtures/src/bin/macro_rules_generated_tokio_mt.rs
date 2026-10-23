//! Bug #5 regression: `#[rudzio::suite]` must register tests produced by
//! `macro_rules!`. Such invocations reach the suite attribute as opaque
//! `Item::Macro` nodes (pre-expansion), so the suite macro cannot see the
//! `#[rudzio::test]` fns they produce — those fns are registered later by the
//! `#[rudzio::test]` attribute itself. This bin exercises the full contract:
//! macro-generated tests compile, run, and are reported alongside a direct fn
//! in the same module, plus an `#[ignore]`-flagged macro-generated test that
//! is reported ignored rather than run.

use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

/// Generate a `#[rudzio::test]` fn that trivially passes.
macro_rules! make_test {
    ($name:ident) => {
        #[rudzio::test]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// Generate a `#[rudzio::test]` fn flagged `#[ignore]`.
macro_rules! make_ignored_test {
    ($name:ident) => {
        #[rudzio::test]
        #[ignore = "macro-generated ignored test"]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    make_test!(macro_generated_a);
    make_test!(macro_generated_b);
    make_ignored_test!(macro_generated_ignored);

    #[rudzio::test]
    async fn direct_fn(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
