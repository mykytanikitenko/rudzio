//! Bug #15 defensive: an inner attribute whose argument is a
//! tool-path (`clippy::...`) must survive. Even though the
//! enclosing attr `#![allow(...)]` is a builtin lint level, the
//! ARGUMENT path has multiple segments which exercises a different
//! syn token tree shape than the simpler single-ident lint name.
//!
//! Direct tool-path inner attributes (e.g. `#![rustfmt::skip]`) are
//! still unstable on Rust stable (gated under
//! `custom_inner_attributes`, tracked at
//! `rust-lang/rust#54726`), so we exercise the closest stable form
//! here.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![allow(
        clippy::pedantic,
        clippy::nursery,
        clippy::style,
        clippy::cargo
    )]

    use super::Test;

    #[rudzio::test]
    fn inner_tool_path_arg_attr_does_not_break_compilation(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
