//! Bug #15 defensive: an `#![warn(...)]` inner attribute on a
//! `#[rudzio::suite]`-annotated mod must survive. `warn` is the
//! companion to `allow` / `deny` and is one of the canonical lint
//! levels; the fix must handle it identically to `allow`.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![warn(clippy::pedantic)]

    use super::Test;

    #[rudzio::test]
    fn inner_warn_does_not_break_compilation(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
