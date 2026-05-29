//! Source rewriter.
//!
//! Walks a parsed [`syn::File`] and mutates the attribute set, signature,
//! and body of every recognised test function; rewrites
//! `#[cfg(test)] mod ...` blocks into `#[rudzio::suite(...)]` blocks. This
//! module does not read or write files — it only mutates syn trees. The
//! [`crate::emit`] module handles I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use proc_macro2::Span;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::visit_mut::{self, VisitMut};
use syn::{
    AttrStyle, Attribute, FnArg, Ident, Item, ItemFn, ItemMod, Meta, ReturnType, Type, UseTree,
    token,
};

use crate::cli::RuntimeChoice;
use crate::detect;
use crate::report::Report;
use crate::test_context::{Plan as TestContextPlan, Resolver as TestContextResolver};

/// Per-attribute decision used by [`Rewriter::strip_companion_test_attrs`]
/// while it walks the fn's attribute list. Two-phase classification keeps
/// the borrow checker happy while still collecting warnings tied back to
/// `&mut self`.
#[non_exhaustive]
enum AttrAction {
    /// Drop without informing the user — used for known-resolved
    /// `#[test_context(...)]` attrs whose work is replaced by the
    /// generated bridge.
    DropResolved,
    /// Drop the attribute. The optional pair carries a span/message
    /// that gets surfaced as a user-visible warning after the borrow
    /// over `attrs` is released.
    DropWithWarning(Option<(Span, String)>),
    /// Leave this attribute on the fn untouched.
    Keep,
}

/// File-wide visitor that broadens every bare `#[cfg_attr(test, ...)]` to
/// `#[cfg_attr(any(test, rudzio_test), ...)]`. Counts the rewrites so the
/// caller can mark the file as changed when at least one fired.
#[non_exhaustive]
struct CfgAttrTestRewriter {
    /// Number of attributes successfully broadened. The caller treats
    /// any non-zero value as "this file changed".
    rewrites: usize,
}

/// Outcome of running [`apply_to_file`] over a single source file.
///
/// Pure aggregation: no I/O implied. The caller decides whether (and how)
/// to surface the changes — the [`crate::emit`] module formats the file
/// back out, [`crate::manifest`] consults `runtimes_used` to widen the
/// crate's Cargo.toml feature set, and so on.
#[derive(Debug)]
#[non_exhaustive]
pub struct Outcome {
    /// True if anything in the file actually changed.
    pub changed: bool,
    /// True if at least one converted fn ended up with an
    /// `::anyhow::Result<()>` return type and therefore needs `anyhow`
    /// pulled into the dep list.
    pub needs_anyhow: bool,
    /// Captured originals of converted fns, keyed by the sentinel index
    /// `N` in `__RUDZIO_MIGRATE_ORIGINAL_PLACEHOLDER_N__`.
    pub original_snippets: Vec<String>,
    /// Set of runtime features this file uses. Unions into the
    /// crate-wide Cargo.toml feature set.
    pub runtimes_used: BTreeSet<RuntimeChoice>,
}

/// Stateful file walker that mutates a [`syn::File`] in place.
///
/// The struct is internal scaffolding: callers use [`apply_to_file`].
/// Field layout is alphabetised so `arbitrary_source_item_ordering`
/// stays happy when fields are added or removed in the future.
struct Rewriter<'res, 'rep> {
    /// Default runtime baked into the suite blocks the rewriter
    /// synthesises when no per-fn flavor forces a different choice.
    default_runtime: RuntimeChoice,
    /// The file path the walker is mutating, retained so warnings can
    /// be attributed to a real on-disk location.
    file_path: PathBuf,
    /// Forced runtimes observed on converted file-scope test fns
    /// (`mod_depth` == 0 at conversion time). Used to pick the runtime
    /// for the synthesised wrapping mod in the post-pass: if every
    /// file-scope fn agrees, honor that choice; otherwise fall back
    /// to `--runtime`.
    file_scope_runtimes: BTreeSet<RuntimeChoice>,
    /// First resolved `#[test_context(T)]` plan seen on a converted
    /// file-scope fn. Used by [`Rewriter::wrap_file_scope_test_fns`] so
    /// the synthesised suite attr points at the generated `CtxBridge`
    /// / `CtxSuite` instead of `common::Test` / `common::Suite` — the
    /// fn sigs were already rewritten to take `&mut CtxBridge`, so
    /// falling back to common types would produce a type mismatch.
    file_scope_test_context_plan: Option<TestContextPlan>,
    /// Depth of the current `mod { ... }` nesting relative to the file
    /// root. 0 means top-level (file scope).
    mod_depth: usize,
    /// True when the caller asked to keep the pre-rewrite source of
    /// every converted fn as a doc-comment sentinel.
    preserve_originals: bool,
    /// Borrow of the caller's report sink so warnings emit during the
    /// walk surface to the user verbatim.
    report: &'rep mut Report,
    /// Aggregated mutation outcome, returned to the caller verbatim.
    rewrite: Outcome,
    /// Owning handle on the pre-rewrite source bytes. Shared with
    /// [`Report`] entries so warnings can underline the offending
    /// span against the original file.
    source: Arc<str>,
    /// True if any `#[test_context(...)]` attr in this file was
    /// stripped. Used by the post-pass to clean up the now-unused
    /// `use test_context::test_context;` import (the function-attr
    /// macro that the user's tests were referring to).
    stripped_any_test_context_attr: bool,
    /// Lookup of generated `Ctx → bridge ident` plans from the
    /// preceding `test_context` discovery pass. Borrowed for the
    /// lifetime of the run.
    test_contexts: &'res TestContextResolver,
}

/// Return-shape classification for a fn's `-> Foo` clause. Drives
/// [`Rewriter::apply_signature_rewrite`]'s decision about whether to
/// wrap the body or leave it verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
enum ReturnKind {
    /// Anything that isn't `()` or a `Result` (e.g. an int return). The
    /// rewriter wraps the body in `Result<(), BoxError>`.
    Other,
    /// A `Result` — left verbatim.
    Result,
    /// `-> ()` written explicitly. Treated identically to
    /// [`Self::UnitImplicit`].
    UnitExplicit,
    /// No return clause at all. The body is left verbatim.
    UnitImplicit,
}

impl Rewriter<'_, '_> {
    /// Rewrite the fn's signature based on its current shape. Empty
    /// param lists are left alone (the `#[rudzio::test]` macro fills
    /// them in at expansion time); non-empty lists without a resolved
    /// `#[test_context]` get a verbatim warning. Non-Result, non-unit
    /// returns get wrapped.
    fn apply_signature_rewrite(&mut self, func: &mut ItemFn, had_resolved_test_context: bool) -> anyhow::Result<()> {
        if func.sig.asyncness.is_none() {
            func.sig.asyncness = Some(token::Async(func.sig.fn_token.span));
        }
        if func.sig.inputs.is_empty() {
            // Zero-param tests are a first-class shape in rudzio — the
            // `#[rudzio::test]` macro accepts them as-is and fills in
            // the missing context at expansion time. Don't synthesize
            // `_ctx: &Test`, which would drag a
            // `use ::rudzio::common::context::Test;` into the mod for
            // no user-visible benefit.
        } else if !had_resolved_test_context {
            // Leave user's params alone — custom context path. Warn
            // only when the tool has *no* independent knowledge of
            // what the param should be (a resolved test_context case
            // is a known-good shape).
            self.warn_span(
                func.sig.ident.span(),
                "test fn has a non-trivial parameter list; preserved verbatim \u{2014} verify the suite's `test = ...` path matches the intended context type",
            );
        } else {
            // Resolved test_context already rewrote the param to the
            // generated bridge — nothing to warn about.
        }

        // FIXME(rs-cleanup): callee returns Result, add ?
        match fn_return_kind(&func.sig.output) {
            ReturnKind::UnitImplicit | ReturnKind::UnitExplicit | ReturnKind::Result => {
                // Leave as-is. `#[rudzio::test]`'s codegen routes the
                // body through `rudzio::IntoRudzioResult`, which has
                // impls for `()` and `Result<...>` — no signature
                // rewrite needed and no `anyhow` dev-dep forced.
            }
            ReturnKind::Other => {
                self.warn_span(
                    func.sig.ident.span(),
                    "test fn returned a non-Result, non-unit type; wrapping in `Result<(), ::rudzio::BoxError>` and discarding the return value",
                );
                let inner: syn::Block = func.block.as_ref().clone();
                let new_block: syn::Block = syn::parse_quote! {{
                    let _unused = { #inner };
                    ::core::result::Result::Ok(())
                }};
                *func.block = new_block;
                func.sig.output =
                    syn::parse_quote! { -> ::core::result::Result<(), ::rudzio::BoxError> };
            }
        };
        Ok(())
}

    /// Build one of the per-ctx wrapper modules emitted by
    /// [`Self::split_module_by_ctx_groups`]. Carries the suite attr,
    /// the `use super::*;` glob, an optional bridge import, and the
    /// pre-bucketed test fns.
    fn build_split_child_mod(
        &self,
        ident: &str,
        ctx_key: Option<&str>,
        runtime_path: &syn::Path,
        fns: Vec<Item>,
    ) -> anyhow::Result<ItemMod> {
        let outer_plan = ctx_key.and_then(|key| self.test_contexts.plan_for(key));
        let (suite_path, test_path) = outer_plan.map_or_else(
            || {
                (
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path("::rudzio::common::context::Suite"),
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path("::rudzio::common::context::Test"),
                )
            },
            |plan| {
                // TODO(rs-cleanup): silencing at line 224 — review and replace with Result-propagating equivalent
                let base = plan.module_path.as_deref().unwrap_or("crate");
                (
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path(&format!("{base}::{}", plan.suite_ident)),
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path(&format!("{base}::{}", plan.bridge_ident)),
                )
            },
        );
        let suite_attr: Attribute = syn::parse_quote! {
            #[::rudzio::suite([
                (
                    runtime = #runtime_path,
                    suite = #suite_path,
                    test = #test_path,
                ),
            ])]
        };
        // TODO(rs-cleanup): masking_arith at line 240 — review and replace with Result-propagating equivalent
        let mut child_items: Vec<Item> = Vec::with_capacity(fns.len().saturating_add(2));
        child_items.push(syn::parse_quote! {
            use super::*;
        });
        if let Some(plan) = outer_plan {
            // TODO(rs-cleanup): silencing at line 245 — review and replace with Result-propagating equivalent
            let base = plan.module_path.as_deref().unwrap_or("crate");
            let bridge_path: syn::Path =
                // FIXME(rs-cleanup): callee returns Result, add ?
                make_static_path(&format!("{base}::{}", plan.bridge_ident));
            child_items.push(syn::parse_quote! {
                use #bridge_path;
            });
        } else {
            child_items.push(syn::parse_quote! {
                use ::rudzio::common::context::Test;
            });
        }
        child_items.extend(fns);
        let mod_ident = Ident::new(ident, Span::call_site());
        let mut child: ItemMod = syn::parse_quote! {
            #suite_attr
            mod #mod_ident {}
        };
        child.content = Some((token::Brace::default(), child_items));
        Ok(child)
    }

    /// Capture the original source bytes spanning the fn so the
    /// caller can render them as a doc-comment sentinel. Returns the
    /// empty string if the recorded byte ranges fall outside the file
    /// (defensive — should not trigger for well-formed input).
    fn capture_original_snippet(&self, func: &ItemFn) -> anyhow::Result<String> {
        let span_start = func
            .attrs
            .iter()
            .map(|attr| attr.span().byte_range().start)
            .min()
            // TODO(rs-cleanup): silencing at line 276 — review and replace with Result-propagating equivalent
            .unwrap_or_else(|| func.sig.fn_token.span.byte_range().start);
        let span_end = func.block.span().byte_range().end;
        let src: &str = &self.source;
        // TODO(rs-cleanup): silencing at line 279 — review and replace with Result-propagating equivalent
        Ok(src.get(span_start..span_end).unwrap_or("").to_owned())
    }

    /// Post-pass: a file that's a TEST BINARY ROOT under `tests/` (i.e.
    /// `tests/<stem>.rs` or `tests/<suite>/mod.rs`) becomes an
    /// independent `[[test]] harness = false` binary after the
    /// migration. Cargo needs a `fn main` in such a binary; append
    /// `#[rudzio::main] fn main() {}` if one isn't already there.
    ///
    /// Submodule files deeper in `tests/` are NOT binary roots —
    /// they're pulled in via `mod` declarations from a root file, so
    /// adding a `fn main` to each would be meaningless at best and
    /// produce double linkme registration at worst.
    fn ensure_tests_binary_has_main(&self, file: &mut syn::File) -> anyhow::Result<()> {
        // FIXME(rs-cleanup): callee returns Result, add ?
        if !is_tests_binary_root(&self.file_path) {
            return Ok(());
        }
        if !self.rewrite.changed {
            return Ok(());
        }
        if file.items.iter().any(item_is_fn_main) {
            return Ok(());
        }
        let main_fn: Item = syn::parse_quote! {
            #[::rudzio::main]
            fn main() {}
        };
        file.items.push(main_fn);
        Ok(())
}

    /// Look up the resolved `#[test_context(T)]` plan on a fn (if any).
    /// Returns the first match — multiple `test_context` attrs on a
    /// single fn aren't a shape rudzio supports.
    fn pop_resolved_test_context_plan(&self, attrs: &[Attribute]) -> anyhow::Result<Option<TestContextPlan>> {
        for attr in attrs {
            if let Some(path) = detect::as_test_context(attr) {
                let key = detect::path_to_string(&path);
                if let Some(plan) = self.test_contexts.plan_for(&key) {
                    return Ok(Some(plan.clone()));
                }
            }
        }
        Ok(None)
    }

    /// Split a single `mod tests { ... }` into one wrapper module per
    /// resolved-ctx group, each with its own `#[rudzio::suite(...)]`.
    /// The outer module keeps `#[cfg(test)]` and any non-test items
    /// (use statements, helpers) so the children can `use super::*;`
    /// to reach them.
    fn split_module_by_ctx_groups(
        &mut self,
        module: &mut ItemMod,
        groups: &BTreeSet<Option<String>>,
        runtime: RuntimeChoice,
        runtime_path: &syn::Path,
    ) -> anyhow::Result<()> {
        let Some((brace, items)) = module.content.take() else {
            return Ok(());
        };
        // Bucket items: non-test items stay in the outer mod;
        // test fns get bucketed by their ctx group.
        let mut shared: Vec<Item> = Vec::new();
        let mut buckets: BTreeMap<Option<String>, Vec<Item>> = groups
            .iter()
            .cloned()
            .map(|key| (key, Vec::new()))
            .collect();
        for item in items {
            let bucket_key = match &item {
                Item::Fn(func)
                    if func
                        .attrs
                        .iter()
                        .any(|attr| detect::classify_test_attr(attr).is_some()) =>
                {
                    Some(func.attrs.iter().find_map(|attr| {
                        let path = detect::as_test_context(attr)?;
                        let key = detect::path_to_string(&path);
                        self.test_contexts.plan_for(&key).map(|_| key)
                    }))
                }
                Item::Const(_)
                | Item::Enum(_)
                | Item::ExternCrate(_)
                | Item::Fn(_)
                | Item::ForeignMod(_)
                | Item::Impl(_)
                | Item::Macro(_)
                | Item::Mod(_)
                | Item::Static(_)
                | Item::Struct(_)
                | Item::Trait(_)
                | Item::TraitAlias(_)
                | Item::Type(_)
                | Item::Union(_)
                | Item::Use(_)
                | Item::Verbatim(_)
                | _ => None,
            };
            if let Some(key) = bucket_key
                && let Some(bucket) = buckets.get_mut(&key)
            {
                bucket.push(item);
            } else {
                shared.push(item);
            }
        }

        let mut new_items = shared;
        for (key, fns) in buckets {
            if fns.is_empty() {
                continue;
            }
            let child_ident = key.as_deref().map_or_else(
                || "tests_default".to_owned(),
                |key_str| format!("tests_with_{}", last_segment_snake(key_str)),
            );
            let child = self.build_split_child_mod(&child_ident, key.as_deref(), runtime_path, fns);
            new_items.push(Item::Mod(child));
        }

        // Hoist any inner attrs on the outer mod to outer-style so the
        // rudzio macros expanded inside the children don't re-encounter
        // them as inner attrs (the children carry their own attrs via
        // the suite generation path below).
        // FIXME(rs-cleanup): callee returns Result, add ?
        hoist_inner_attrs_on_mod(module);
        module.content = Some((brace, new_items));
        let _: bool = self.rewrite.runtimes_used.insert(runtime);
        self.rewrite.changed = true;
        self.warn_span(
            module.ident.span(),
            "module mixed `#[test_context(...)]` and plain tests; split into per-context child modules so each suite tuple has the right `test = ...` path. Suite blocks remain inside the original `#[cfg(test)] mod` for dev-dep visibility.",
        );
        Ok(())
}

    /// Strip companion attributes (`#[should_panic]`,
    /// `#[test_context(...)]`) and report on each. Returns true if any
    /// resolved `#[test_context]` was stripped — the caller uses that
    /// to decide whether to rewrite the fn's first param into a
    /// generated bridge.
    fn strip_companion_test_attrs(&mut self, attrs: &mut Vec<Attribute>) -> anyhow::Result<bool> {
        // Two-phase: first classify each attr and record the warnings
        // we'd emit, then retain. This avoids borrowing `self` inside
        // the `retain` closure.
        let mut actions: Vec<AttrAction> = Vec::with_capacity(attrs.len());
        let mut had_resolved_test_context = false;
        for attr in attrs.iter() {
            if detect::is_should_panic_attr(attr) {
                actions.push(AttrAction::DropWithWarning(Some((
                    attr.span(),
                    "#[should_panic] stripped; rudzio does not support panic-expectation \u{2014} rewrite the body to assert the panic manually".to_owned(),
                ))));
                continue;
            }
            if let Some(path) = detect::as_test_context(attr) {
                self.stripped_any_test_context_attr = true;
                let key = detect::path_to_string(&path);
                if self.test_contexts.plan_for(&key).is_some() {
                    had_resolved_test_context = true;
                    actions.push(AttrAction::DropResolved);
                } else {
                    actions.push(AttrAction::DropWithWarning(Some((
                        attr.span(),
                        format!(
                            "#[test_context({key})] stripped without generating a bridge: no `impl AsyncTestContext for {key}` was found in this crate. Finish the migration by hand."
                        ),
                    ))));
                }
                continue;
            }
            actions.push(AttrAction::Keep);
        }
        for action in &actions {
            if let AttrAction::DropWithWarning(Some((span, msg))) = action {
                self.warn_span(*span, msg.clone());
            }
        }
        let mut iter = actions.into_iter();
        attrs.retain(|_attr| matches!(iter.next(), Some(AttrAction::Keep)));
        Ok(had_resolved_test_context)
    }

    /// Convert a single `#[...test...]` fn into a `#[::rudzio::test]`
    /// fn: rewrite the test attribute, strip companion attrs, possibly
    /// rewrite the ctx parameter, normalise the signature shape.
    fn try_convert_fn(&mut self, func: &mut ItemFn) -> anyhow::Result<()> {
        let matched_idx_and_kind = func
            .attrs
            .iter()
            .enumerate()
            .find_map(|(i, attr)| detect::classify_test_attr(attr).map(|kind| (i, kind)));
        let Some((idx, detected)) = matched_idx_and_kind else {
            return Ok(());
        };
        // FIXME(rs-cleanup): callee returns Result, add ?
        if has_self_receiver(func) {
            self.warn_span(
                func.sig.ident.span(),
                "test fn takes `self` receiver; rudzio tests are free fns \u{2014} skipping",
            );
            return Ok(());
        }
        // Any rstest marker on the fn (or its params) → skip. The
        // rstest model spreads across multiple attribute sites (the
        // outer `#[rstest]` wrapper, inline `#[case(...)]`,
        // `#[values(...)]` on params), and only some of them are
        // visible in `func.attrs`; check both the fn-level attrs and
        // the param-level attrs.
        if func.attrs.iter().any(detect::is_rstest_attr)
            || func.sig.inputs.iter().any(fn_arg_has_rstest_attr)
        {
            self.warn_span(
                func.sig.ident.span(),
                "test fn uses rstest (`#[rstest]` / `#[case]` / `#[values]`); left unchanged \u{2014} rudzio has no parameterised-test equivalent, rewrite by hand",
            );
            return Ok(());
        }
        // Multi-param or non-reference-param test fns are almost
        // certainly `rstest`-style parameterised tests (the `#[case]`
        // or `#[values]` family), which rudzio does not support.
        // Converting them would make rudzio's signature transform
        // paste `<'_, R>` onto things like `&str`, yielding "lifetime
        // and type arguments are not allowed on builtin type `str`".
        // Skip with a warning and let the user rewrite.
        // FIXME(rs-cleanup): callee returns Result, add ?
        if has_non_ctx_shaped_params(func) {
            self.warn_span(
                func.sig.ident.span(),
                "test fn has parameters that don't look like a single `&T` / `&mut T` context borrow (likely rstest #[case] / #[values]); left unchanged \u{2014} rudzio has no parameterised-test equivalent, rewrite by hand",
            );
            return Ok(());
        }

        for extra in &detected.extra_tokio_args {
            self.warn_span(
                func.sig.ident.span(),
                format!("#[tokio::test] arg `{extra}` dropped; rudzio does not forward it"),
            );
        }
        if let Some(msg) = detected.kind.needs_compat_warning() {
            self.warn_span(func.sig.ident.span(), msg);
        }

        let original_snippet = self
            .preserve_originals
            .then(|| self.capture_original_snippet(func));

        // FIXME(rs-cleanup): callee returns Result, add ?
        replace_attr_with_rudzio_test(&mut func.attrs, idx);
        let resolved_plan = self.pop_resolved_test_context_plan(&func.attrs);
        let had_resolved_test_context = resolved_plan.is_some();
        let _stripped: bool = self.strip_companion_test_attrs(&mut func.attrs);
        if let Some(plan) = resolved_plan.as_ref() {
            // FIXME(rs-cleanup): callee returns Result, add ?
            rewrite_ctx_param_to_bridge(func, &plan.ctx_ident, &plan.bridge_ident);
            if self.mod_depth == 0 && self.file_scope_test_context_plan.is_none() {
                self.file_scope_test_context_plan = Some(plan.clone());
            }
        }
        self.apply_signature_rewrite(func, had_resolved_test_context);

        // Pick the runtime: forced by detected kind > file's default.
        let runtime = detected
            .kind
            .forced_runtime()
            // TODO(rs-cleanup): silencing at line 541 — review and replace with Result-propagating equivalent
            .unwrap_or(self.default_runtime);
        let _: bool = self.rewrite.runtimes_used.insert(runtime);
        if self.mod_depth == 0
            && let Some(forced) = detected.kind.forced_runtime()
        {
            let _: bool = self.file_scope_runtimes.insert(forced);
        }

        if let Some(snippet) = original_snippet {
            let snippet_idx = self.rewrite.original_snippets.len();
            self.rewrite.original_snippets.push(snippet);
            // Leading space so prettyplease emits `/// __RUDZIO..._N__`
            // (with a visible gap) rather than `///__RUDZIO..._N__`.
            let sentinel = format!(" __RUDZIO_MIGRATE_ORIGINAL_PLACEHOLDER_{snippet_idx}__");
            let attr: Attribute = syn::parse_quote! { #[doc = #sentinel] };
            func.attrs.insert(0, attr);
        }

        self.rewrite.changed = true;
        self.report.add_converted(1);
        Ok(())
}

    /// If the module is a `#[cfg(test)]`-gated test module containing
    /// at least one recognised test fn, rewrite its outer attrs into
    /// the rudzio shape: broaden the cfg to
    /// `#[cfg(any(test, rudzio_test))]` and append a
    /// `#[::rudzio::suite(...)]` attribute. Mixed-context modules
    /// (multiple `#[test_context]` types, or `test_context` mixed with
    /// plain tests) get split via
    /// [`Self::split_module_by_ctx_groups`] instead.
    fn try_promote_cfg_test_mod(&mut self, module: &mut ItemMod) -> anyhow::Result<()> {
        // Declaration-only `mod tests;` (no inline body) can't be
        // wrapped with `#[rudzio::suite]` — the macro expects an
        // inline block to descend into.
        if module.content.is_none() {
            return Ok(());
        }
        // Only promote modules that actually contain at least one
        // recognized test fn. A `#[cfg(test)]` module with just helper
        // fns (no `#[test]`) must stay a plain `cfg(test)` module;
        // wrapping it with `#[rudzio::suite]` would fail the macro's
        // "at least one #[rudzio::test]" assertion.
        // FIXME(rs-cleanup): callee returns Result, add ?
        if !module_has_any_test_fn(module) {
            return Ok(());
        }
        // FIXME(rs-cleanup): callee returns Result, add ?
        if has_rudzio_suite(&module.attrs) {
            return Ok(());
        }
        // Only promote modules whose OWN attrs include `#[cfg(test)]`.
        // A plain `pub mod outer { #[cfg(test)] mod tests { ... } }`
        // would otherwise trigger: `module_has_any_test_fn(outer)` is
        // true (recursive), so `outer` would get the suite attr even
        // though it isn't test-gated and its non-test items live in
        // the normal lib build. The recursive visit descends into
        // `outer` and catches the inner cfg(test) mod there.
        if !module.attrs.iter().any(is_cfg_test_attr) {
            return Ok(());
        }
        // Expand bare `#[cfg(test)]` to `#[cfg(any(test, rudzio_test))]`
        // so the module compiles under BOTH per-crate `cargo test` AND
        // the `cargo rudzio test` aggregator (which depends on the
        // crate as a regular lib, not a `--test` target, and activates
        // `--cfg rudzio_test` via `RUSTFLAGS`). `cfg(test)` stays in
        // the disjunction so dev-dep visibility is unchanged under
        // plain `cargo test`. Compound cfgs such as
        // `#[cfg(all(test, feature = "mock"))]` are untouched —
        // rewriting those is out of v1 scope; user handles them by
        // hand.
        // FIXME(rs-cleanup): callee returns Result, add ?
        rewrite_cfg_test_to_cfg_any(&mut module.attrs);

        // Pick the runtime: if every recognized test fn in the module
        // forces the same runtime (e.g. all `#[tokio::test(flavor = ...)]`
        // with the same flavor), honor that unanimous choice;
        // otherwise fall back to `--runtime`.
        // TODO(rs-cleanup): silencing at line 615 — review and replace with Result-propagating equivalent
        // FIXME(rs-cleanup): callee returns Result, add ?
        let runtime = unanimous_inner_runtime(module).unwrap_or(self.default_runtime);
        // FIXME(rs-cleanup): callee returns Result, add ?
        let runtime_path = make_static_path(runtime.suite_path());

        // Mixed-context handling: when a single `mod tests` holds both
        // plain `#[tokio::test]` fns and `#[test_context(Ctx)]` fns
        // (or two distinct ctx types), one shared suite attr would
        // type-mismatch half the fns. Split into per-ctx child modules
        // instead.
        // FIXME(rs-cleanup): callee returns Result, add ?
        let groups = group_test_fns_by_ctx(module, self.test_contexts);
        if groups.len() > 1 {
            self.split_module_by_ctx_groups(module, &groups, runtime, &runtime_path);
            return Ok(());
        }

        // Choose suite/test paths based on whether any fn inside uses
        // a resolved `#[test_context(T)]`.
        // FIXME(rs-cleanup): callee returns Result, add ?
        let resolved_ctx = first_resolved_test_context(module, self.test_contexts);
        let (suite_path, test_path) = if let Some(plan) = resolved_ctx {
            // TODO(rs-cleanup): silencing at line 633 — review and replace with Result-propagating equivalent
            let base = plan.module_path.as_deref().unwrap_or("crate");
            if plan.module_path.is_none() {
                self.warn_span(
                    module.ident.span(),
                    format!(
                        "bridge for `{}` was generated in `{}`, which isn't reachable from `crate::` (typically because it lives under `tests/`). Emitted `crate::{}` as a best-effort placeholder \u{2014} adjust the `suite = ...` / `test = ...` paths by hand to match your test binary's module tree.",
                        plan.ctx_ident,
                        plan.impl_file.display(),
                        plan.suite_ident,
                    ),
                );
            }
            (
                // FIXME(rs-cleanup): callee returns Result, add ?
                make_static_path(&format!("{base}::{}", plan.suite_ident)),
                // FIXME(rs-cleanup): callee returns Result, add ?
                make_static_path(&format!("{base}::{}", plan.bridge_ident)),
            )
        } else {
            (
                // FIXME(rs-cleanup): callee returns Result, add ?
                make_static_path("::rudzio::common::context::Suite"),
                // FIXME(rs-cleanup): callee returns Result, add ?
                make_static_path("::rudzio::common::context::Test"),
            )
        };
        let suite_attr: Attribute = syn::parse_quote! {
            #[::rudzio::suite([
                (
                    runtime = #runtime_path,
                    suite = #suite_path,
                    test = #test_path,
                ),
            ])]
        };
        // `#[rudzio::suite(...)]` is an attribute macro on `mod`; its
        // expansion rejects `#![inner]` attrs inside the module body
        // with "an inner attribute is not permitted in this context".
        // Hoist each inner attr to an outer attr on the `mod` item
        // itself — for the common cases (`#![allow(clippy::…)]`,
        // `#![deny(…)]`, `#![expect(…)]`, etc.) this is semantically
        // equivalent: the lints propagate into the body either way.
        // FIXME(rs-cleanup): callee returns Result, add ?
        hoist_inner_attrs_on_mod(module);
        // Place the suite attribute as the LAST outer attribute on the
        // `mod` item (i.e. immediately before the `mod` keyword)
        // rather than at position 0. Two reasons:
        //   • `#[cfg(test)]` must evaluate before the macro runs. In
        //     a non-test build, cfg(test) prunes the item so the
        //     attribute macro never expands — avoiding references to
        //     rudzio (a dev-dep) from non-test compilations.
        //   • Hoisted `#[expect(...)]` / `#[allow(...)]` attrs need to
        //     sit outside the macro so the lints apply to the
        //     expanded code just like they did in the original body.
        // `push` ends up after any pre-existing outer attrs (cfg,
        // expect, user-written lints) and after the hoisted inner
        // attrs — exactly the ordering the user expects.
        module.attrs.push(suite_attr);
        let _: bool = self.rewrite.runtimes_used.insert(runtime);
        self.rewrite.changed = true;
        Ok(())
}

    /// Surface a span-attached warning to the report sink. Centralises
    /// the byte-range translation so callers don't repeat it.
    fn warn_span(&mut self, span: Span, message: impl Into<String>) -> anyhow::Result<()> {
        let range = span.byte_range();
        let offset = range.start;
        // TODO(rs-cleanup): masking_arith at line 695 — review and replace with Result-propagating equivalent
        let len = range.end.saturating_sub(range.start);
        self.report.warn_with_span(
            self.file_path.clone(),
            span.start().line,
            offset,
            len,
            Arc::clone(&self.source),
            message,
        );
        Ok(())
}

    /// Post-pass: pull every converted file-scope `#[rudzio::test]`
    /// fn into a synthesised `#[cfg(test)] #[rudzio::suite([...])] mod
    /// tests { ... }` so rudzio's runner actually sees them (a bare
    /// `#[rudzio::test]` at file scope registers no tokens and leaves
    /// the `Test` type unresolved).
    fn wrap_file_scope_test_fns(&mut self, file: &mut syn::File) -> anyhow::Result<()> {
        let indices: Vec<usize> = file
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                if let Item::Fn(func) = item
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    && fn_has_rudzio_test_attr(func)
                {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        let Some(&first_idx) = indices.first() else {
            return Ok(());
        };
        let mut fns: Vec<Item> = Vec::with_capacity(indices.len());
        for &i in indices.iter().rev() {
            fns.push(file.items.remove(i));
        }
        fns.reverse();

        let runtime = if self.file_scope_runtimes.len() == 1 {
            self.file_scope_runtimes
                .iter()
                .next()
                .copied()
                // TODO(rs-cleanup): silencing at line 740 — review and replace with Result-propagating equivalent
                .unwrap_or(self.default_runtime)
        } else {
            self.default_runtime
        };
        // FIXME(rs-cleanup): callee returns Result, add ?
        let runtime_path = make_static_path(runtime.suite_path());
        // If any of the wrapped fns had a resolved `#[test_context(T)]`,
        // their signatures were already rewritten to `&mut CtxBridge`.
        // Pointing the suite attr at `common::Suite` / `common::Test`
        // in that case would produce a type mismatch the moment
        // rudzio's macro tries to resolve `Test<'tc>` against the fn's
        // param type. Use the generated bridge paths instead.
        let (suite_path, test_path) = self.file_scope_test_context_plan.as_ref().map_or_else(
            || {
                (
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path("::rudzio::common::context::Suite"),
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path("::rudzio::common::context::Test"),
                )
            },
            |plan| {
                // TODO(rs-cleanup): silencing at line 759 — review and replace with Result-propagating equivalent
                let base = plan.module_path.as_deref().unwrap_or("crate");
                (
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path(&format!("{base}::{}", plan.suite_ident)),
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    make_static_path(&format!("{base}::{}", plan.bridge_ident)),
                )
            },
        );

        // The synth module is already `#[cfg(test)]`, so the fns we
        // just pulled in no longer need their own `#[cfg(test)]` attr
        // — strip it to keep the generated code clean.
        let cleaned_fns: Vec<Item> = fns
            .into_iter()
            .map(|item| {
                if let Item::Fn(mut func) = item {
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    func.attrs.retain(|attr| !is_cfg_test_attr(attr));
                    Item::Fn(func)
                } else {
                    item
                }
            })
            .collect();

        // TODO(rs-cleanup): masking_arith at line 782 — review and replace with Result-propagating equivalent
        let mut synth_items: Vec<Item> = Vec::with_capacity(cleaned_fns.len().saturating_add(3));
        synth_items.push(syn::parse_quote! {
            use super::*;
        });
        if let Some(plan) = &self.file_scope_test_context_plan {
            // Bring the bridge ident into scope so the wrapped fn
            // sigs (`&mut <Ctx>RudzioBridge`) resolve. Use the
            // inferred module path when available, falling back to
            // `crate::` with a warning (emitted once per plan at
            // module-promotion time; repeating it here would be
            // noise).
            // TODO(rs-cleanup): silencing at line 793 — review and replace with Result-propagating equivalent
            let base = plan.module_path.as_deref().unwrap_or("crate");
            let bridge_path: syn::Path =
                // FIXME(rs-cleanup): callee returns Result, add ?
                make_static_path(&format!("{base}::{}", plan.bridge_ident));
            synth_items.push(syn::parse_quote! {
                use #bridge_path;
            });
        }
        synth_items.extend(cleaned_fns);

        // The synth mod is named `tests` (snake_case), so the
        // suite-macro's `__rudzio_run_test_<ModName>_…` expansion also
        // stays snake_case — no `#[allow(non_snake_case)]` needed.
        let mut synth: ItemMod = syn::parse_quote! {
            #[cfg(any(test, rudzio_test))]
            #[::rudzio::suite([
                (
                    runtime = #runtime_path,
                    suite = #suite_path,
                    test = #test_path,
                ),
            ])]
            mod tests {}
        };
        synth.content = Some((token::Brace::default(), synth_items));
        file.items.insert(first_idx, Item::Mod(synth));

        let _: bool = self.rewrite.runtimes_used.insert(runtime);
        self.rewrite.changed = true;
        Ok(())
}
}

impl VisitMut for CfgAttrTestRewriter {
    #[inline]
    fn visit_attribute_mut(&mut self, i: &mut Attribute) -> anyhow::Result<()> {
        // FIXME(rs-cleanup): callee returns Result, add ?
        if rewrite_cfg_attr_test_attr(i) {
            // TODO(rs-cleanup): masking_arith at line 828 — review and replace with Result-propagating equivalent
            self.rewrites = self.rewrites.saturating_add(1);
        }
        // FIXME(rs-cleanup): callee returns Result, add ?
        visit_mut::visit_attribute_mut(self, i);
        Ok(())
}
}

impl VisitMut for Rewriter<'_, '_> {
    #[inline]
    fn visit_file_mut(&mut self, i: &mut syn::File) -> anyhow::Result<()> {
        for item in &mut i.items {
            if let Item::Mod(module) = item {
                self.try_promote_cfg_test_mod(module);
            }
        }
        // FIXME(rs-cleanup): callee returns Result, add ?
        visit_mut::visit_file_mut(self, i);
        self.wrap_file_scope_test_fns(i);
        self.ensure_tests_binary_has_main(i);
        if self.stripped_any_test_context_attr {
            // FIXME(rs-cleanup): callee returns Result, add ?
            prune_test_context_macro_imports(i);
        };
        Ok(())
}

    #[inline]
    fn visit_item_fn_mut(&mut self, i: &mut ItemFn) -> anyhow::Result<()> {
        // Warn on rstest-family markers independently of whether a
        // `#[test]` attribute is also present — rstest sometimes
        // replaces `#[test]` outright, and those fns would otherwise
        // slip through unwarned.
        if i.attrs.iter().any(detect::is_rstest_attr)
            || i.sig.inputs.iter().any(fn_arg_has_rstest_attr)
        {
            self.warn_span(
                i.sig.ident.span(),
                "test fn uses rstest (`#[rstest]` / `#[case]` / `#[values]`); left unchanged \u{2014} rudzio has no parameterised-test equivalent, rewrite by hand",
            );
            return Ok(());
        }
        // File-scope test fns are now converted and later wrapped in
        // a synthesised `#[cfg(test)] #[rudzio::suite(...)] mod { ... }`
        // by the post-pass in `visit_file_mut`. See
        // `wrap_file_scope_test_fns`.
        self.try_convert_fn(i);
        // FIXME(rs-cleanup): callee returns Result, add ?
        visit_mut::visit_item_fn_mut(self, i);
        Ok(())
}

    #[inline]
    fn visit_item_mod_mut(&mut self, i: &mut ItemMod) -> anyhow::Result<()> {
        self.try_promote_cfg_test_mod(i);
        // TODO(rs-cleanup): masking_arith at line 876 — review and replace with Result-propagating equivalent
        self.mod_depth = self.mod_depth.saturating_add(1);
        // FIXME(rs-cleanup): callee returns Result, add ?
        visit_mut::visit_item_mod_mut(self, i);
        // TODO(rs-cleanup): masking_arith at line 878 — review and replace with Result-propagating equivalent
        self.mod_depth = self.mod_depth.saturating_sub(1);
        // FIXME(rs-cleanup): callee returns Result, add ?
        prune_unused_test_context_import(i);
        if self.stripped_any_test_context_attr
            && let Some((_, items)) = &mut i.content
        {
            // FIXME(rs-cleanup): callee returns Result, add ?
            prune_test_context_macro_imports_in_items(items);
        };
        Ok(())
}
}

/// Run the rewriter over a single parsed file.
///
/// `source` is the pre-rewrite UTF-8 byte stream, retained inside the
/// returned [`Outcome`] only when the caller asked to preserve
/// originals. `default_runtime` is used as the suite runtime whenever
/// no per-fn flavor forces a different choice. `test_contexts` carries
/// the bridge-plan lookup table from the prior discovery pass.
#[inline]
pub fn apply(
    source: Arc<str>,
    file: &mut syn::File,
    default_runtime: RuntimeChoice,
    preserve_originals: bool,
    test_contexts: &TestContextResolver,
    file_path: &Path,
    report: &mut Report,
) -> anyhow::Result<Outcome> {
    let mut walker = Rewriter {
        default_runtime,
        file_path: file_path.to_path_buf(),
        file_scope_runtimes: BTreeSet::new(),
        file_scope_test_context_plan: None,
        mod_depth: 0,
        preserve_originals,
        report,
        rewrite: Outcome {
            changed: false,
            needs_anyhow: false,
            original_snippets: Vec::new(),
            runtimes_used: BTreeSet::new(),
        },
        source,
        stripped_any_test_context_attr: false,
        test_contexts,
    };
    walker.visit_file_mut(file);
    // FIXME(rs-cleanup): callee returns Result, add ?
    let cfg_attr_rewrites = rewrite_cfg_attr_test_in_file(file);
    if cfg_attr_rewrites > 0 {
        walker.rewrite.changed = true;
    }
    Ok(walker.rewrite)
}

/// Collect every `forced_runtime()` reported by `classify_test_attr` on
/// fns nested directly or transitively under `items`. Used by
/// [`unanimous_inner_runtime`] to decide whether a module's fns agree.
fn collect_runtime_hints(items: &[Item], out: &mut BTreeSet<RuntimeChoice>) -> anyhow::Result<()> {
    for item in items {
        if let Item::Fn(func) = item {
            for attr in &func.attrs {
                if let Some(detected) = detect::classify_test_attr(attr)
                    && let Some(runtime) = detected.kind.forced_runtime()
                {
                    let _: bool = out.insert(runtime);
                }
            }
            continue;
        }
        if let Item::Mod(sub) = item
            && let Some((_, inner)) = &sub.content
        {
            // FIXME(rs-cleanup): callee returns Result, add ?
            collect_runtime_hints(inner, out);
        }
    };
    Ok(())
}

/// Recursively remove a `test_context` leaf or grouped item from a
/// `UseTree` rooted under `test_context::`. Returns `true` if the
/// caller's containing tree is now empty (e.g. an empty Group).
fn drop_test_context_leaf(tree: &mut UseTree) -> anyhow::Result<bool> {
    Ok(match tree {
        UseTree::Name(name) if name.ident == "test_context" => true,
        UseTree::Rename(rename) if rename.ident == "test_context" => true,
        UseTree::Group(group) => {
            // `Punctuated` doesn't expose `retain_mut`; rebuild via a
            // Vec round-trip. The Group's items get reassembled with
            // the same comma punctuation.
            let kept: Vec<UseTree> = mem::take(&mut group.items)
                .into_iter()
                .filter_map(|mut inner| {
                    // FIXME(rs-cleanup): callee returns Result, add ?
                    if drop_test_context_leaf(&mut inner) {
                        None
                    } else {
                        Some(inner)
                    }
                })
                .collect();
            for inner in kept {
                group.items.push(inner);
            }
            group.items.is_empty()
        }
        // FIXME(rs-cleanup): callee returns Result, add ?
        UseTree::Path(path_use) => drop_test_context_leaf(&mut path_use.tree),
        UseTree::Glob(_) | UseTree::Name(_) | UseTree::Rename(_) => false,
    })
}

/// First fn in `module` that carries a resolved `#[test_context(T)]`
/// attribute. Used by [`Rewriter::try_promote_cfg_test_mod`] to decide
/// whether to point the synthesised suite attr at a generated bridge
/// or fall back to the common Test/Suite types.
fn first_resolved_test_context<'res>(
    module: &ItemMod,
    resolver: &'res TestContextResolver,
) -> anyhow::Result<Option<&'res TestContextPlan>> {
    let Some((_, items)) = &module.content else {
        return Ok(None);
    };
    for item in items {
        if let Item::Fn(func) = item {
            for attr in &func.attrs {
                if let Some(path) = detect::as_test_context(attr) {
                    let key = detect::path_to_string(&path);
                    if let Some(plan) = resolver.plan_for(&key) {
                        return Ok(Some(plan));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// True if any of the fn's parameters carries an rstest-family
/// attribute (`#[case]`, `#[values]`, ...). Inspects both `Typed` and
/// `Receiver` shapes.
fn fn_arg_has_rstest_attr(arg: &FnArg) -> anyhow::Result<bool> {
    Ok(match arg {
        FnArg::Typed(pat_type) => pat_type.attrs.iter().any(detect::is_rstest_attr),
        FnArg::Receiver(recv) => recv.attrs.iter().any(detect::is_rstest_attr),
    })
}

/// True if the fn carries a `#[::rudzio::test]` (or unprefixed
/// `#[rudzio::test]`) outer attribute. Used by the file-scope wrap
/// post-pass to spot fns it has already converted.
fn fn_has_rudzio_test_attr(func: &ItemFn) -> anyhow::Result<bool> {
    Ok(func.attrs.iter().any(|attr| {
        let path = detect::path_to_string(attr.path());
        path == "::rudzio::test" || path == "rudzio::test"
    }))
}

/// Classify a fn's `-> Foo` clause into a [`ReturnKind`]. Best-effort:
/// only the leaf segment is consulted, so any type whose last segment
/// is `Result` is treated as a Result.
fn fn_return_kind(ret: &ReturnType) -> anyhow::Result<ReturnKind> {
    let ReturnType::Type(_, ty) = ret else {
        return Ok(ReturnKind::UnitImplicit);
    };
    if let Type::Tuple(tuple) = ty.as_ref()
        && tuple.elems.is_empty()
    {
        return Ok(ReturnKind::UnitExplicit);
    }
    if let Type::Path(path_ty) = ty.as_ref() {
        let last = path_ty
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
            // TODO(rs-cleanup): silencing at line 1049 — review and replace with Result-propagating equivalent
            .unwrap_or_default();
        return Ok(if last == "Result" {
            ReturnKind::Result
        } else {
            ReturnKind::Other
        });
    }
    Ok(ReturnKind::Other)
}

/// Group every recognised test fn inside `module` by its target
/// context (None for plain tests, `Some(ctx_key)` for resolved
/// `#[test_context]` uses). The set's len is what
/// [`Rewriter::try_promote_cfg_test_mod`] uses to decide single-suite
/// vs split — len == 1 is the single case, len > 1 triggers the
/// per-ctx split.
fn group_test_fns_by_ctx(
    module: &ItemMod,
    resolver: &TestContextResolver,
) -> anyhow::Result<BTreeSet<Option<String>>> {
    let mut groups: BTreeSet<Option<String>> = BTreeSet::new();
    let Some((_, items)) = &module.content else {
        return Ok(groups);
    };
    for item in items {
        if let Item::Fn(func) = item {
            if !func
                .attrs
                .iter()
                .any(|attr| detect::classify_test_attr(attr).is_some())
            {
                continue;
            }
            let key = func.attrs.iter().find_map(|attr| {
                let path = detect::as_test_context(attr)?;
                let key_str = detect::path_to_string(&path);
                resolver.plan_for(&key_str).map(|_| key_str)
            });
            let _: bool = groups.insert(key);
        }
    }
    Ok(groups)
}

/// True if the fn has params that don't match the "single ctx borrow"
/// shape rudzio expects: exactly zero params (we leave the fn alone)
/// or exactly one param that's a `&T` / `&mut T` reference (user's own
/// ctx type, left alone — `rudzio::test`'s macro transform will fix
/// the generics). Anything else — multiple params, owned values,
/// tuples — signals a parameterised-test library (rstest, etc.) whose
/// shape the rudzio macro can't handle.
fn has_non_ctx_shaped_params(func: &ItemFn) -> anyhow::Result<bool> {
    let n = func.sig.inputs.len();
    if n == 0 {
        return Ok(false);
    }
    if n > 1 {
        return Ok(true);
    }
    let Some(arg) = func.sig.inputs.first() else {
        return Ok(false);
    };
    let FnArg::Typed(pat_type) = arg else {
        return Ok(true);
    };
    Ok(!matches!(&*pat_type.ty, Type::Reference(_)))
}

/// True if the attribute list already carries a `#[rudzio::suite(...)]`
/// (in either prefixed or unprefixed form). Used to short-circuit the
/// promote pass on already-migrated modules.
fn has_rudzio_suite(attrs: &[Attribute]) -> anyhow::Result<bool> {
    Ok(attrs.iter().any(|attr| {
        detect::path_to_string(attr.path()) == "rudzio::suite"
            || detect::path_to_string(attr.path()) == "::rudzio::suite"
    }))
}

/// True if any of the fn's parameters is a `self` receiver. Receivers
/// disqualify the fn from rudzio test conversion — rudzio tests are
/// free fns.
fn has_self_receiver(func: &ItemFn) -> anyhow::Result<bool> {
    Ok(func.sig
        .inputs
        .iter()
        .any(|arg| matches!(arg, FnArg::Receiver(_))))
}

/// Rewrite every inner attribute on `module` to outer style. Used to
/// pre-condition `#[rudzio::suite]`'s expansion target — the macro
/// rejects `#![inner]` attrs.
fn hoist_inner_attrs_on_mod(module: &mut ItemMod) -> anyhow::Result<()> {
    for attr in &mut module.attrs {
        if matches!(attr.style, AttrStyle::Inner(_)) {
            attr.style = AttrStyle::Outer;
        }
    };
    Ok(())
}

/// True if the attribute is exactly `#[cfg(test)]` (no compound
/// predicate). Compound forms (`all(test, ...)`, `any(test, ...)`,
/// feature gates) are intentionally excluded.
fn is_cfg_test_attr(attr: &Attribute) -> anyhow::Result<bool> {
    if detect::path_to_string(attr.path()) != "cfg" {
        return Ok(false);
    }
    let Meta::List(list) = &attr.meta else {
        return Ok(false);
    };
    Ok(list.tokens.to_string().trim() == "test")
}

/// True if `path` is a test-binary root under `tests/`. Two shapes
/// count: a direct child `tests/<stem>.rs`, or a suite-dir
/// `tests/<suite>/mod.rs`. Anything deeper is a submodule and inherits
/// its main from the root via `mod` declarations.
fn is_tests_binary_root(path: &Path) -> anyhow::Result<bool> {
    let mut components: Vec<&OsStr> = path.components().map(Component::as_os_str).collect();
    let Some(tests_idx) = components
        .iter()
        .position(|seg| *seg == OsStr::new("tests"))
    else {
        return Ok(false);
    };
    // TODO(rs-cleanup): masking_arith at line 1173 — review and replace with Result-propagating equivalent
    let rel = components.split_off(tests_idx.saturating_add(1));
    // `tests/<stem>.rs`: exactly one `.rs` component after `tests/`.
    if let [single] = rel.as_slice() {
        return Ok(Path::new(single).extension().is_some_and(|ext| ext == "rs"));
    }
    // `tests/<suite>/mod.rs`: exactly two components, last is `mod.rs`.
    Ok(matches!(rel.as_slice(), [_, last] if *last == OsStr::new("mod.rs")))
}

/// True if any sub-tree of `item` (recursively, descending into
/// modules) carries a recognised test attribute on a fn.
fn item_has_any_test_fn(item: &Item) -> anyhow::Result<bool> {
    if let Item::Fn(func) = item {
        return Ok(func
            .attrs
            .iter()
            .any(|attr| detect::classify_test_attr(attr).is_some()));
    }
    if let Item::Mod(module) = item {
        // FIXME(rs-cleanup): callee returns Result, add ?
        return Ok(module_has_any_test_fn(module));
    }
    Ok(false)
}

/// True if the item is `fn main`. Used by the test-binary main
/// post-pass to decide whether to synthesise one.
fn item_is_fn_main(item: &Item) -> anyhow::Result<bool> {
    Ok(if let Item::Fn(func) = item {
        func.sig.ident == "main"
    } else {
        false
    })
}

/// Convert `crate::foo::DbCtx` → `db_ctx`. Used to derive a snake-case
/// suffix for split-child module names (`tests_with_db_ctx`).
fn last_segment_snake(path: &str) -> anyhow::Result<String> {
    // TODO(rs-cleanup): silencing at line 1210 — review and replace with Result-propagating equivalent
    let last = path.rsplit("::").next().unwrap_or(path);
    // TODO(rs-cleanup): masking_arith at line 1211 — review and replace with Result-propagating equivalent
    let mut out = String::with_capacity(last.len().saturating_add(4));
    for (i, ch) in last.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

/// Parse a path string we synthesised ourselves. Returns the literal
/// `crate` path on parse failure — the inputs to this fn are always
/// well-formed in practice (built up from validated identifiers), so
/// the fallback exists strictly to keep the rewriter free of
/// `.expect()` calls.
fn make_static_path(text: &str) -> anyhow::Result<syn::Path> {
    // TODO(rs-cleanup): silencing at line 1231 — review and replace with Result-propagating equivalent
    Ok(syn::parse_str::<syn::Path>(text).unwrap_or_else(|_err| syn::parse_quote!(crate)))
}

/// True if `module` (recursively) contains at least one fn carrying a
/// recognised test attribute.
fn module_has_any_test_fn(module: &ItemMod) -> anyhow::Result<bool> {
    let Some((_, items)) = &module.content else {
        return Ok(false);
    };
    Ok(items.iter().any(item_has_any_test_fn))
}

/// File-wide variant of [`prune_test_context_macro_imports_in_items`].
fn prune_test_context_macro_imports(file: &mut syn::File) -> anyhow::Result<()> {
    // FIXME(rs-cleanup): callee returns Result, add ?
    prune_test_context_macro_imports_in_items(&mut file.items);
    Ok(())
}

/// After every `#[test_context(...)]` attribute is stripped, the
/// `use test_context::test_context;` (or grouped equivalent) the
/// user's tests imported is dead. Walk every `use test_context::...`
/// item and drop any leaf named `test_context` — leaving the trait
/// (`AsyncTestContext`, `TestContext`) imports intact.
fn prune_test_context_macro_imports_in_items(items: &mut Vec<Item>) -> anyhow::Result<()> {
    items.retain_mut(|item| {
        let Item::Use(use_item) = item else {
            return true;
        };
        let root_test_context_match =
            matches!(&use_item.tree, UseTree::Path(path) if path.ident == "test_context");
        if !root_test_context_match {
            return true;
        }
        if let UseTree::Path(path) = &mut use_item.tree {
            // FIXME(rs-cleanup): callee returns Result, add ?
            let _empty = drop_test_context_leaf(&mut path.tree);
        }
        // FIXME(rs-cleanup): callee returns Result, add ?
        !tree_is_empty(&use_item.tree)
    });
    Ok(())
}

/// Removes `use test_context::test_context;` imports inside the module
/// once all `#[test_context(...)]` attributes have been stripped — they
/// were what referenced the attribute macro. Leaves other
/// `test_context` re-exports (e.g. the trait `AsyncTestContext` itself)
/// alone.
fn prune_unused_test_context_import(module: &mut ItemMod) -> anyhow::Result<()> {
    let Some((_, items)) = &mut module.content else {
        return Ok(());
    };
    items.retain(|item| {
        let Item::Use(use_item) = item else {
            return true;
        };
        let tokens = quote::ToTokens::to_token_stream(&use_item.tree).to_string();
        // Canonical form prettyplease emits for `use test_context::test_context;`.
        let normalized: String = tokens.split_whitespace().collect::<Vec<_>>().join("");
        !(normalized == "test_context::test_context"
            || normalized == "::test_context::test_context")
    });
    Ok(())
}

/// Replace the indexed attribute with a freshly-parsed
/// `#[::rudzio::test]`. Used as the tail-end of the conversion path
/// inside [`Rewriter::try_convert_fn`].
fn replace_attr_with_rudzio_test(attrs: &mut [Attribute], idx: usize) -> anyhow::Result<()> {
    let new_attr: Attribute = syn::parse_quote! { #[::rudzio::test] };
    if let Some(slot) = attrs.get_mut(idx) {
        *slot = new_attr;
    };
    Ok(())
}

/// Rewrite a single `#[cfg_attr(test, ...)]` attribute to
/// `#[cfg_attr(any(test, rudzio_test), ...)]`. Returns true iff the
/// attribute matched the bare-test shape and was rewritten. Compound
/// predicates and feature gates are left alone.
fn rewrite_cfg_attr_test_attr(attr: &mut Attribute) -> anyhow::Result<bool> {
    if !attr.path().is_ident("cfg_attr") {
        return Ok(false);
    }
    let Meta::List(list) = &attr.meta else {
        return Ok(false);
    };
    let Ok(metas): Result<Punctuated<Meta, syn::Token![,]>, _> =
        list.parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated)
    else {
        return Ok(false);
    };
    let first_is_bare_test = matches!(
        metas.first(),
        Some(Meta::Path(path)) if path.is_ident("test")
    );
    if !first_is_bare_test {
        return Ok(false);
    }
    let rest: Vec<&Meta> = metas.iter().skip(1).collect();
    let rebuilt: Attribute = if rest.is_empty() {
        // `#[cfg_attr(test)]` — degenerate; still rewrite predicate.
        syn::parse_quote!(#[cfg_attr(any(test, rudzio_test))])
    } else {
        syn::parse_quote!(#[cfg_attr(any(test, rudzio_test), #(#rest),*)])
    };
    *attr = rebuilt;
    Ok(true)
}

/// File-wide pass that rewrites every bare `#[cfg_attr(test, ...)]` to
/// `#[cfg_attr(any(test, rudzio_test), ...)]`. Motivation: a struct in
/// `src/**` carrying `#[cfg_attr(test, derive(fake::Dummy))]` is used
/// by `#[cfg(any(test, rudzio_test))] mod tests` in the same or
/// another file of the same crate. Under `cargo rudzio test` the
/// aggregator compiles the crate as a plain lib with `--cfg
/// rudzio_test` (no `cfg(test)`), so the derive doesn't fire and the
/// test fails to typecheck. Broadening the predicate makes the
/// conditional attr active under both `cargo test` and
/// `cargo rudzio test`.
///
/// Only the bare `cfg_attr(test, ...)` shape is rewritten. Compound
/// predicates (`all(test, ...)`, `any(test, ...)`, feature gates) are
/// left alone — same v1-scope policy as
/// [`rewrite_cfg_test_to_cfg_any`]. Returns the number of attributes
/// rewritten so the caller can mark the file as changed.
fn rewrite_cfg_attr_test_in_file(file: &mut syn::File) -> anyhow::Result<usize> {
    let mut walker = CfgAttrTestRewriter { rewrites: 0 };
    walker.visit_file_mut(file);
    Ok(walker.rewrites)
}

/// Rewrite every bare `#[cfg(test)]` in `attrs` to
/// `#[cfg(any(test, rudzio_test))]`. Idempotent: compound cfgs
/// (including an already-rewritten `any(test, rudzio_test)`) do not
/// match [`is_cfg_test_attr`], so running this twice is a no-op.
fn rewrite_cfg_test_to_cfg_any(attrs: &mut [Attribute]) -> anyhow::Result<()> {
    for attr in attrs.iter_mut() {
        // FIXME(rs-cleanup): callee returns Result, add ?
        if is_cfg_test_attr(attr) {
            *attr = syn::parse_quote!(#[cfg(any(test, rudzio_test))]);
        }
    };
    Ok(())
}

/// Rewrite the first parameter of `func` from `&CtxIdent` to
/// `&BridgeIdent`, in place. No-op when the param shape doesn't match
/// — keeping the rewriter conservative on hand-rolled signatures.
fn rewrite_ctx_param_to_bridge(func: &mut ItemFn, ctx_ident: &str, bridge_ident: &str) -> anyhow::Result<()> {
    let Some(first) = func.sig.inputs.first_mut() else {
        return Ok(());
    };
    let FnArg::Typed(pat_type) = first else {
        return Ok(());
    };
    let Type::Reference(type_ref) = &mut *pat_type.ty else {
        return Ok(());
    };
    let Type::Path(type_path) = &mut *type_ref.elem else {
        return Ok(());
    };
    let Some(last) = type_path.path.segments.last_mut() else {
        return Ok(());
    };
    if last.ident == ctx_ident {
        last.ident = Ident::new(bridge_ident, last.ident.span());
    };
    Ok(())
}

/// True if `tree` is empty after a [`drop_test_context_leaf`] pass.
/// Used to decide whether to drop the entire `use` item.
fn tree_is_empty(tree: &UseTree) -> anyhow::Result<bool> {
    Ok(match tree {
        // FIXME(rs-cleanup): callee returns Result, add ?
        UseTree::Path(path_use) => tree_is_empty(&path_use.tree),
        UseTree::Group(group) => group.items.is_empty(),
        UseTree::Glob(_) | UseTree::Name(_) | UseTree::Rename(_) => false,
    })
}

/// If every recognised test fn under `module` agrees on a forced
/// runtime, return it. Otherwise return None — the caller falls back
/// to the run's `--runtime`.
fn unanimous_inner_runtime(module: &ItemMod) -> anyhow::Result<Option<RuntimeChoice>> {
    let Some((_, items)) = &module.content else {
        return Ok(None);
    };
    let mut runtimes: BTreeSet<RuntimeChoice> = BTreeSet::new();
    // FIXME(rs-cleanup): callee returns Result, add ?
    collect_runtime_hints(items, &mut runtimes);
    Ok(if runtimes.len() == 1 {
        runtimes.into_iter().next()
    } else {
        None
    })
}
