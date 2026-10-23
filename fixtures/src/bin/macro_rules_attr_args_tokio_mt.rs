//! Bug #5 edge case: `#[rudzio::test(...)]` attribute arguments emitted
//! through `macro_rules!` must round-trip into the per-config registration
//! codegen exactly as they do for direct fns.
//!
//! Two argument shapes are exercised:
//!   * `#[ignore = "..."]` — the generated test is reported ignored;
//!   * `#[rudzio::test(timeout = 1)]` — the per-test body timeout fires and
//!     the generated test is reported timed out (proving the `timeout = <n>`
//!     argument reaches `extract_test_attr_args` through the forwarding
//!     macro, not just the bare-attribute path).

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

/// Generate a `#[rudzio::test]` fn flagged `#[ignore = "..."]`.
macro_rules! make_ignored {
    ($name:ident) => {
        #[rudzio::test]
        #[ignore = "macro-generated ignored test"]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

/// Generate a `#[rudzio::test(timeout = 1)]` fn whose body sleeps past the
/// timeout so the attribute-driven per-test body timeout fires.
macro_rules! make_timeout {
    ($name:ident) => {
        #[rudzio::test(timeout = 1)]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            ::tokio::time::sleep(::std::time::Duration::from_secs(5)).await;
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

    make_ignored!(attr_ignored);
    make_timeout!(attr_timeout);
}

#[rudzio::main]
fn main() {}
