//! Bug #15 defensive: multiple consecutive inner attributes at the
//! top of a `#[rudzio::suite]`-annotated mod must ALL survive macro
//! expansion. The partition logic must handle a sequence, not just a
//! single attribute.

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
    #![allow(clippy::unwrap_used)]
    #![warn(clippy::nursery)]
    //! Followed by an inner doc comment to mix attribute kinds.

    use super::Test;

    #[rudzio::test]
    fn multiple_inner_attrs_do_not_break_compilation(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
