//! Bug #15 defensive: an `#![deny(...)]` inner attribute on a
//! `#[rudzio::suite]`-annotated mod must survive. `deny` escalates a
//! lint to a hard error inside the mod scope; the fixture body does
//! NOT trigger the chosen lint, so the test passes iff macro expansion
//! preserves the attribute at a position rustc accepts as an inner
//! attribute.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![deny(unused_variables)]

    use super::Test;

    #[rudzio::test]
    fn inner_deny_does_not_break_compilation(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
