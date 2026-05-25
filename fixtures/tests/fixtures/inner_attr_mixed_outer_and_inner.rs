//! Bug #15 defensive: outer attribute on the mod (`#[cfg(any(...))]`
//! placed before `#[rudzio::suite]`) PLUS an inner attribute inside
//! the mod body — the macro must partition them by `attr.style`,
//! emitting the outer one in front of the regenerated `mod` block
//! and the inner one INSIDE the regenerated body.

use rudzio::common::context::Test;

#[cfg(unix)]
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
    fn mixed_outer_and_inner_attrs_compile(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
