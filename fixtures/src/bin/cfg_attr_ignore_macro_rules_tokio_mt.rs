//! `#[cfg_attr(cond, ignore = "...")]` emitted through `macro_rules!`.
//!
//! The `#[rudzio::test]` attribute re-parses the generated fn and re-registers
//! it through the hidden forwarding macro, so the `cfg_attr(cond, ignore)`
//! reason must round-trip through the same `extract_ignore_reason` path as a
//! direct fn. `all()` is always true, so this generated test is deterministically
//! ignored and the integration test asserts the ignore reason is reported.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

/// Generate a `#[rudzio::test]` fn carrying a `#[cfg_attr(all(), ignore = "...")]`.
macro_rules! make_cfg_attr_ignored {
    ($name:ident) => {
        #[cfg_attr(all(), ignore = "macro-generated cfg_attr ignore")]
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

    make_cfg_attr_ignored!(cfg_attr_ignored_via_macro);
}

#[rudzio::main]
fn main() {}
