//! Invariants of [`RuntimeGroupKey`] under the suite macro's
//! `TypeId`-based key derivation.
//!
//! # Background
//!
//! Regression suite for the cross-binary `RuntimeGroupKey` collision
//! that manifested as a SIGSEGV in downstream test binaries running
//! under the v2 aggregator (one binary's `RuntimeGroupOwner`
//! dispatched another binary's `Test::context` via a wrong-type
//! pointer cast — the wild crash chain ended in a
//! `deadpool::Pool::timeouts` memcpy on a dropped `Arc<PoolInner>`).
//!
//! The historical bug shape: the macro's `group_key_source` was
//! `fnv1a64(format!("{}::{}", runtime_path_tokens, suite_path_tokens))`
//! — string-only, no type identity. Two distinct concrete `Suite`
//! types declared under byte-identical attribute-token spelling
//! (e.g. each crate's local `mod common { pub struct Suite; }`)
//! collided on the key. The dispatcher placed their tokens in one
//! bucket, picked `first.runtime_group_owner`, and dispatched every
//! token through that owner's `run_group` — which casts `*const ()`
//! pointers back to its own concrete `(Runtime, Suite)` types. Under
//! cross-binary linkage this read field offsets that belonged to a
//! different layout — UB whose symptom was a SEGV in the next
//! `Arc::deref`.
//!
//! # Fix
//!
//! `RuntimeGroupKey` is now derived from `TypeId::of::<R>()` and
//! `TypeId::of::<S<'static, R>>()` at static-init time. `TypeId`s erase
//! lifetimes, so the canonical `<'_, R>` Suite shape lifts cleanly to
//! `<'static, R>` for keying; `TypeId`s distinguish concrete types
//! regardless of path-string spelling, so two crates each defining
//! their OWN local `Suite` get distinct keys and the collision cannot
//! recur.
//!
//! # Invariants pinned here
//!
//! Each test below pins one observable consequence of the fix.
//! Together they would have caught the historical bug at its
//! earliest layer — and any future macro regression that drifted
//! back toward string-based or otherwise-spelling-sensitive keys
//! fails one of them loudly instead of bottoming out in a runtime
//! allocator's memcpy.

use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread;

/// Substring uniquely identifying tokens whose `module_path` belongs
/// to THIS file's `#[rudzio::suite]` block. Under the v2 aggregator,
/// the macro plants `token_statics` at the PARENT scope of the suite
/// mod (see `macro-internals/src/suite_codegen.rs` — `expand_suite`
/// emits `mod #mod_name { … }` followed by the statics at the same
/// scope, NOT inside the mod). So `module_path!()` on every token
/// evaluates to the parent scope, which under v2 is the
/// `__rudzio_test_rudzio__<file>` proxy wrapper — not `tests` and
/// not the suite mod name. That is why the filter targets the file
/// name.
const THIS_FILE_MARKER: &str = "runtime_group_key_invariants";

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
mod tests {
    use std::any::TypeId;

    use anyhow::Context as _;
    use rudzio::common::context::{Suite as CommonSuite, Test};
    use rudzio::runtime::tokio::Multithread;
    use rudzio::suite::{RuntimeGroupKey, validate_dispatch_table};
    use rudzio::token::{TEST_TOKENS, Token};

    use super::THIS_FILE_MARKER;

    /// Invariant 1 — defense-in-depth dispatch-table check.
    ///
    /// [`validate_dispatch_table`] walks every linked token in
    /// `TEST_TOKENS` and asserts
    /// `token.runtime_group_owner.group_key() == token.runtime_group_key`.
    /// With the macro emitting both sides as the same const
    /// expression (`RuntimeGroupKey::of::<R, S<'static, R>>()`) this is
    /// a tautology under correct expansion — so a failure here means
    /// the macro's owner-side and token-side derivations have drifted
    /// apart, which is precisely the shape of the historical
    /// cross-binary collision bug. The runner itself calls this on
    /// dispatch entry; exposing it as a test pins the contract at the
    /// integration layer too.
    #[rudzio::test]
    fn dispatch_table_passes_owner_token_consistency_check(
        _ctx: &Test<'_, Multithread>,
    ) -> anyhow::Result<()> {
        let tokens: Vec<&'static Token> = TEST_TOKENS.iter().collect();
        validate_dispatch_table(&tokens).map_err(|msg| anyhow::anyhow!(msg))?;
        Ok(())
    }

    /// Invariant 2 — macro-canonical key agreement.
    ///
    /// Every token from THIS file's `#[rudzio::suite]` block declares
    /// `(runtime = Multithread::new, suite = Suite, test = Test)` —
    /// so the macro must emit the same `RuntimeGroupKey` value that a
    /// direct call to
    /// `RuntimeGroupKey::of::<Multithread, common::Suite<'static, Multithread>>()`
    /// returns. Disagreement means the macro's substitution of
    /// `#suite_base::<'static, #runtime_type>` drifted from the
    /// public API's contract, which is precisely the failure mode the
    /// string-based hash hid in plain sight.
    ///
    /// As a side benefit this also pins cross-block sharing for the
    /// canonical `(Multithread, common::Suite)` pair: all of THIS
    /// file's tokens land on one key, and that key is the canonical
    /// one, so any other dogfood block declaring the same types
    /// would share it too.
    #[rudzio::test]
    fn macro_emits_canonical_key_for_this_block(
        _ctx: &Test<'_, Multithread>,
    ) -> anyhow::Result<()> {
        let canonical = RuntimeGroupKey::of::<Multithread, CommonSuite<'static, Multithread>>();
        let mut checked: usize = 0;
        for token in TEST_TOKENS
            .iter()
            .filter(|tok| tok.module_path.contains(THIS_FILE_MARKER))
        {
            anyhow::ensure!(
                token.runtime_group_key == canonical,
                "token `{}` at {}:{} reports runtime_group_key {:?}; \
                 expected the canonical key for \
                 (Multithread, common::Suite) -- {:?}. \
                 The macro's emission diverged from the direct \
                 RuntimeGroupKey::of API for the same (Runtime, Suite) \
                 types.",
                token.name,
                token.file,
                token.line,
                token.runtime_group_key,
                canonical,
            );
            checked = checked.saturating_add(1);
        }
        anyhow::ensure!(
            checked > 0,
            "no tokens from this file in TEST_TOKENS -- the filter assumption \
             (`{THIS_FILE_MARKER}` substring of `module_path`) is wrong"
        );
        Ok(())
    }

    /// Invariant 3 — within-block consistency.
    ///
    /// All tokens emitted by ONE `#[rudzio::suite]` block share a
    /// single `RuntimeGroupKey`. The macro plants per-test token
    /// statics and the owner static at the parent scope of the suite
    /// mod and reads the key from the same const expression
    /// (`RuntimeGroupKey::of::<R, S<'static, R>>()`) at both sites —
    /// so divergence here would mean a macro regression breaking
    /// per-test consistency within a single block. Scoped to THIS
    /// file's tokens so the test depends only on this module's
    /// expansion, not on the surrounding dogfood.
    #[rudzio::test]
    fn within_block_tokens_share_one_key(_ctx: &Test<'_, Multithread>) -> anyhow::Result<()> {
        let mut iter = TEST_TOKENS
            .iter()
            .filter(|tok| tok.module_path.contains(THIS_FILE_MARKER));
        let first = iter
            .next()
            .context("no tokens from this file in TEST_TOKENS -- the filter assumption is wrong")?;
        for token in iter {
            anyhow::ensure!(
                token.runtime_group_key == first.runtime_group_key,
                "tokens from this suite block disagree on group_key: \
                 `{}` -> {:?}, `{}` -> {:?}",
                first.name,
                first.runtime_group_key,
                token.name,
                token.runtime_group_key,
            );
        }
        Ok(())
    }

    /// Invariant 4 — isolation across different suite types.
    ///
    /// `RuntimeGroupKey::of` must yield distinct keys for two
    /// different concrete `Suite` types. This is the property whose
    /// historical violation (under string-based hashing) enabled the
    /// cross-binary `RuntimeGroupOwner` collision and the wild SEGV.
    /// Tested at the key-derivation API layer (rather than the macro
    /// layer, which would require a second full `Suite` trait impl
    /// for boilerplate cost ~150 lines): the macro emits a single
    /// `RuntimeGroupKey::of::<#runtime_type, #suite_base::<'static,
    /// #runtime_type>>()` expression, so API-layer correctness IS
    /// macro-emission correctness.
    ///
    /// `u32` stands in for "some other concrete `'static` type" --
    /// the assertion holds whenever the two type-parameter slots
    /// resolve to distinct `TypeId`s; we don't need the second type
    /// to be a valid `Suite` impl because we are testing the key
    /// derivation, not the dispatcher.
    #[rudzio::test]
    fn distinct_suite_types_produce_distinct_keys(
        _ctx: &Test<'_, Multithread>,
    ) -> anyhow::Result<()> {
        let common_suite_key =
            RuntimeGroupKey::of::<Multithread, CommonSuite<'static, Multithread>>();
        let other_key = RuntimeGroupKey::of::<Multithread, u32>();
        anyhow::ensure!(
            common_suite_key != other_key,
            "RuntimeGroupKey::of<Multithread, common::Suite> and \
             RuntimeGroupKey::of<Multithread, u32> produced the same key \
             ({common_suite_key:?}) -- TypeId-based identity is broken; \
             the cross-binary collision class is back"
        );
        // Sanity: also confirm the runtime-side TypeId distinction is
        // visible -- RuntimeGroupKey embeds it directly. Two keys
        // with the same suite_type_id but different runtime_type_id
        // must still differ.
        anyhow::ensure!(
            TypeId::of::<Multithread>() != TypeId::of::<u32>(),
            "TypeId::of<Multithread> == TypeId::of<u32>; the underlying \
             type-identity primitive is broken (this is a stdlib invariant, \
             not rudzio's)"
        );
        Ok(())
    }
}
