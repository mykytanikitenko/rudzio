//! Bug #15 defensive: inner attribute on the mod PLUS outer
//! attributes on individual items inside the mod. The partition logic
//! must distinguish "mod-level inner attrs" (lifted into the
//! regenerated mod body) from "item-level outer attrs" (left attached
//! to the item).

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

    /// Doc-comment on a sibling helper fn inside the mod -- an
    /// item-level outer attribute that must NOT get lifted along with
    /// the mod-level inner attr above. Sanity-check that the macro's
    /// partition doesn't touch attrs attached to items inside the mod.
    #[inline(always)]
    fn helper(value: u32) -> u32 {
        value
    }

    #[rudzio::test]
    fn outer_attrs_on_items_inside_mod_compile(_ctx: &Test) -> anyhow::Result<()> {
        let _ = helper(42);
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
