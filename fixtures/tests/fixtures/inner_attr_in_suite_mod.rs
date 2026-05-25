//! Bug #15 regression: `#[rudzio::suite]` must preserve inner
//! attributes (`#![expect(...)]`, `#![allow(...)]`) inside the mod
//! body — they apply to the contents of the mod and must continue to
//! after the macro reshape.
//!
//! Pre-fix the macro splatted ALL of `input_mod.attrs` (which holds
//! both outer-style and inner-style attrs in a single Vec) before
//! the regenerated `mod #mod_name { ... }`. The inner attrs ended up
//! at the call site between sibling top-level items — not a context
//! that accepts inner attrs — so rustc errored with
//! "an inner attribute is not permitted in this context".
//!
//! Post-fix the macro partitions by `attr.style`: outer attrs stay
//! before the `mod`; inner attrs are emitted INSIDE the regenerated
//! mod body where they belong.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![expect(
        clippy::panic_in_result_fn,
        reason = "this fixture asserts inner attrs survive the `#[rudzio::suite]` macro reshape; the lint name itself is arbitrary"
    )]

    use super::Test;

    #[rudzio::test]
    fn inner_attr_does_not_break_compilation(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
