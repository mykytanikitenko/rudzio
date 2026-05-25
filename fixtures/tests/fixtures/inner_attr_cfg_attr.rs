//! Bug #15 defensive: an `#![cfg_attr(...)]` inner attribute (which
//! expands to another inner attribute based on a cfg predicate) must
//! survive the suite macro. `cfg_attr` has a non-trivial meta tree
//! and the partition logic must not get confused by the nested
//! attribute syntax.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![cfg_attr(any(test, rudzio_test), allow(unused_variables))]

    use super::Test;

    #[rudzio::test]
    fn inner_cfg_attr_does_not_break_compilation(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
