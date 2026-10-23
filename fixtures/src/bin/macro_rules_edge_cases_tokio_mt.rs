//! Bug #5 edge cases: `#[rudzio::suite]` must register every shape of
//! `macro_rules!`-generated test, not just the single-trivial-fn case the
//! original regression fixture covered. Each edge below maps to a distinct
//! mechanism the two-phase fix (`#[rudzio::test]` re-registration via the
//! hidden per-suite `__rudzio_register_test!` forwarding macro) must honour:
//!
//!   * multiple invocations emitting the same body under different names;
//!   * one invocation emitting TWO test fns;
//!   * nested macros (outer invokes inner which emits `#[rudzio::test]`);
//!   * the macro itself defined at crate level, outside the suite mod;
//!   * a `&mut Test` context parameter and a no-context parameter — the same
//!     shapes direct fns support;
//!   * a non-test helper fn emitted alongside the test that calls it.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

/// Same trivial body, parameterised name. Three invocations must produce
/// three distinct, independently-registered tests.
macro_rules! make_same_body {
    ($name:ident) => {
        #[rudzio::test]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// One invocation emits two test fns (different names).
macro_rules! make_pair {
    ($first:ident, $second:ident) => {
        #[rudzio::test]
        async fn $first(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
        #[rudzio::test]
        async fn $second(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// Inner macro emitting a `#[rudzio::test]` fn; the outer macro delegates to
/// it so the test fn reaches the suite through two macro-expansion layers.
macro_rules! make_inner {
    ($name:ident) => {
        #[rudzio::test]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// Outer macro that delegates to [`make_inner`] so a `#[rudzio::test]` fn is
/// produced through two expansion layers (nested-macro case).
macro_rules! make_outer {
    ($name:ident) => {
        make_inner!($name);
    };
}

/// `&mut Test` context parameter — the mutable-borrow shape direct fns
/// support must round-trip through `macro_rules!` unchanged.
macro_rules! make_mut_ctx {
    ($name:ident) => {
        #[rudzio::test]
        async fn $name(_ctx: &mut Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// No context parameter — the bare `async fn` shape direct fns support.
macro_rules! make_no_ctx {
    ($name:ident) => {
        #[rudzio::test]
        async fn $name() -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// Emits a non-test helper fn alongside a test that calls it. The helper
/// must remain a plain fn (not a registered test) while the test registers
/// and runs, proving the forwarding macro does not swallow non-test items.
macro_rules! make_with_helper {
    ($name:ident) => {
        const fn macro_generated_helper() -> u32 {
            42
        }
        #[rudzio::test]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                macro_generated_helper() == 42,
                "helper must be callable from the macro-generated test",
            );
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

    // Same body, different names.
    make_same_body!(same_body_a);
    make_same_body!(same_body_b);
    make_same_body!(same_body_c);

    // Two test fns from one invocation.
    make_pair!(pair_first, pair_second);

    // Nested macro expansion.
    make_outer!(nested_inner);

    // &mut context and no-context parameter shapes.
    make_mut_ctx!(mut_ctx_test);
    make_no_ctx!(no_ctx_test);

    // Helper fn emitted alongside the test.
    make_with_helper!(calls_helper);
}

#[rudzio::main]
fn main() {}
