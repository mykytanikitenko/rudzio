//! Bug #15 defensive: an `#![allow(...)]` inner attribute at the top
//! of a `#[rudzio::suite]`-annotated mod must survive macro expansion.
//! Pre-fix the macro splatted all of `input_mod.attrs` outside the
//! regenerated `mod` block; `allow` styled as `#![...]` lands at
//! invalid position between sibling items and rustc rejects it.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![allow(clippy::pedantic)]

    use super::Test;

    #[rudzio::test]
    fn inner_allow_does_not_break_compilation(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
