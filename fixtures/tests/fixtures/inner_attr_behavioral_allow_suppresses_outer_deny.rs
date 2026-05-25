//! Bug #15 BEHAVIORAL defensive: not only must the inner attribute
//! survive macro expansion, it must actually BIND to the mod body so
//! lint suppression behaves the way the user wrote it.
//!
//! Test shape:
//!  * File-level `#![deny(unused_variables)]` escalates the rustc
//!    `unused_variables` lint to a hard error throughout the file.
//!  * Inside the suite-wrapped mod, `#![allow(unused_variables)]`
//!    OVERRIDES the file-level deny for items inside the mod.
//!  * A deliberately-unused `let unused: i32 = 42;` (no leading
//!    underscore — would otherwise be exempt from the lint) sits
//!    inside the test body.
//!
//! Outcomes:
//!  * Inner attr preserved AND bound to mod body → mod-level `allow`
//!    suppresses the lint inside the mod → compile passes.
//!  * Inner attr dropped entirely → file-level deny fires on `unused`
//!    → compile fails.
//!  * Inner attr preserved but at wrong scope (e.g. emitted again at
//!    file level) → also compile passes BUT for the wrong reason —
//!    the same allow now suppresses lints outside the mod too. The
//!    sibling `outer_function` deliberately contains NO triggering
//!    code so that wrong-scope and right-scope can't be distinguished
//!    by this fixture alone; the inverse case (sibling code that
//!    SHOULD lint) is covered by
//!    `inner_attr_behavioral_outer_deny_still_fires_outside_mod.rs`.

#![deny(unused_variables)]

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![allow(unused_variables)]

    use super::Test;

    #[rudzio::test]
    fn inner_allow_suppresses_outer_deny_inside_mod_body(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        // No leading underscore: would fire `unused_variables` lint.
        // Under the file-level `deny` + mod-level `allow`, this MUST
        // compile if the inner attribute is preserved AND bound at
        // the mod body scope.
        let unused: i32 = 42;
        let _ = unused;
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
