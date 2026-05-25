//! Bug #15 defensive: an inner doc comment (`//! ...`) inside a
//! `#[rudzio::suite]`-annotated mod must survive. Inner doc comments
//! desugar to `#![doc = "..."]` inner attributes, so they share the
//! same partition path as inner lint attrs; this fixture exercises
//! the doc-attribute variant specifically.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    //! Inner doc comment describing this test module. Desugars to
    //! `#![doc = "..."]` — exercises the inner-attr partition for
    //! the doc-attribute path.

    use super::Test;

    #[rudzio::test]
    fn inner_doc_comment_does_not_break_compilation(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
