//! Source rewriter. Walks a parsed `syn::File` and mutates the
//! attribute set, signature, and body of every recognised test
//! function; rewrites `#[cfg(test)] mod ...` blocks into
//! `#[rudzio::suite(...)]` blocks.
//!
//! This module does not read or write files — it only mutates syn
//! trees. The `emit` module handles I/O.

use std::collections::BTreeSet;
use std::sync::Arc;

use proc_macro2::TokenStream;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::token;
use syn::visit_mut::{self, VisitMut};
use syn::{Attribute, Expr, ExprBlock, FnArg, Item, ItemFn, ItemMod, Meta, ReturnType, Stmt, Type};

use crate::cli::RuntimeChoice;
use crate::detect;
use crate::report::Report;
use crate::test_context::{TestContextPlan, TestContextResolver};

#[derive(Debug)]
pub struct FileRewrite {
    /// True if anything in the file actually changed.
    pub changed: bool,
    /// Set of runtime features this file uses. Unions into the
    /// crate-wide Cargo.toml feature set.
    pub runtimes_used: BTreeSet<RuntimeChoice>,
    /// True if at least one converted fn ended up with an
    /// `::anyhow::Result<()>` return type and therefore needs
    /// `anyhow` pulled into the dep list.
    pub needs_anyhow: bool,
    /// Captured originals of converted fns, keyed by the sentinel
    /// index `N` in `__RUDZIO_MIGRATE_ORIGINAL_PLACEHOLDER_N__`.
    pub original_snippets: Vec<String>,
}

pub fn rewrite_file(
    source: Arc<String>,
    file: &mut syn::File,
    default_runtime: RuntimeChoice,
    preserve_originals: bool,
    test_contexts: &TestContextResolver,
    file_path: &std::path::Path,
    report: &mut Report,
) -> FileRewrite {
    let mut walker = Rewriter {
        default_runtime,
        preserve_originals,
        source,
        test_contexts,
        file_path: file_path.to_path_buf(),
        report,
        rewrite: FileRewrite {
            changed: false,
            runtimes_used: BTreeSet::new(),
            needs_anyhow: false,
            original_snippets: Vec::new(),
        },
        mod_depth: 0,
        file_scope_runtimes: BTreeSet::new(),
        file_scope_test_context_plan: None,
        stripped_any_test_context_attr: false,
    };
    walker.visit_file_mut(file);
    let cfg_attr_rewrites = rewrite_cfg_attr_test_in_file(file);
    if cfg_attr_rewrites > 0 {
        walker.rewrite.changed = true;
    }
    walker.rewrite
}

struct Rewriter<'a, 'r> {
    default_runtime: RuntimeChoice,
    preserve_originals: bool,
    source: Arc<String>,
    test_contexts: &'a TestContextResolver,
    file_path: std::path::PathBuf,
    report: &'r mut Report,
    rewrite: FileRewrite,
    /// Depth of the current `mod { ... }` nesting relative to the
    /// file root. 0 means top-level (file scope).
    mod_depth: usize,
    /// Forced runtimes observed on converted file-scope test fns
    /// (mod_depth == 0 at conversion time). Used to pick the runtime
    /// for the synthesized wrapping mod in the post-pass: if every
    /// file-scope fn agrees, honor that choice; otherwise fall back
    /// to `--runtime`.
    file_scope_runtimes: BTreeSet<RuntimeChoice>,
    /// First resolved `#[test_context(T)]` plan seen on a converted
    /// file-scope fn. Used by `wrap_file_scope_test_fns` so the
    /// synthesized suite attr points at the generated `CtxBridge` /
    /// `CtxSuite` instead of `common::Test` / `common::Suite` — the
    /// fn sigs were already rewritten to take `&mut CtxBridge`,
    /// so falling back to common types would produce a type
    /// mismatch.
    file_scope_test_context_plan: Option<TestContextPlan>,
    /// True if any `#[test_context(...)]` attr in this file was
    /// stripped. Used by the post-pass to clean up the now-unused
    /// `use test_context::test_context;` import (the function-attr
    /// macro that the user's tests were referring to).
    stripped_any_test_context_attr: bool,
}

impl VisitMut for Rewriter<'_, '_> {
    fn visit_file_mut(&mut self, file: &mut syn::File) {
        for item in &mut file.items {
            if let Item::Mod(m) = item {
                self.try_promote_cfg_test_mod(m);
            }
        }
        visit_mut::visit_file_mut(self, file);
        self.wrap_file_scope_test_fns(file);
        self.ensure_tests_binary_has_main(file);
        if self.stripped_any_test_context_attr {
            prune_test_context_macro_imports(file);
        }
    }

    fn visit_item_mod_mut(&mut self, m: &mut ItemMod) {
        self.try_promote_cfg_test_mod(m);
        self.mod_depth = self.mod_depth.saturating_add(1);
        visit_mut::visit_item_mod_mut(self, m);
        self.mod_depth = self.mod_depth.saturating_sub(1);
        prune_unused_test_context_import(m);
        if self.stripped_any_test_context_attr {
            if let Some((_, items)) = &mut m.content {
                prune_test_context_macro_imports_in_items(items);
            }
        }
    }

    fn visit_item_fn_mut(&mut self, f: &mut ItemFn) {
        // Warn on rstest-family markers independently of whether a
        // `#[test]` attribute is also present — rstest sometimes
        // replaces `#[test]` outright, and those fns would otherwise
        // slip through unwarned.
        if f.attrs.iter().any(detect::is_rstest_attr)
            || f.sig.inputs.iter().any(fn_arg_has_rstest_attr)
        {
            self.warn_span(
                f.sig.ident.span(),
                "test fn uses rstest (`#[rstest]` / `#[case]` / `#[values]`); left unchanged — rudzio has no parameterised-test equivalent, rewrite by hand",
            );
            return;
        }
        // File-scope test fns are now converted and later wrapped in a
        // synthesized `#[cfg(test)] #[rudzio::suite(...)] mod { ... }`
        // by the post-pass in `visit_file_mut`. See
        // `wrap_file_scope_test_fns`.
        self.try_convert_fn(f);
        visit_mut::visit_item_fn_mut(self, f);
    }
}

impl Rewriter<'_, '_> {
    /// Split a single `mod tests { ... }` into one wrapper module
    /// per resolved-ctx group, each with its own
    /// `#[rudzio::suite(...)]`. The outer module keeps `#[cfg(test)]`
    /// and any non-test items (use statements, helpers) so the
    /// children can `use super::*;` to reach them.
    fn split_module_by_ctx_groups(
        &mut self,
        m: &mut ItemMod,
        groups: std::collections::BTreeMap<Option<String>, ()>,
        runtime: RuntimeChoice,
        runtime_path: &syn::Path,
    ) {
        let Some((brace, items)) = m.content.take() else {
            return;
        };
        // Bucket items: non-test items stay in the outer mod;
        // test fns get bucketed by their ctx group.
        let mut shared: Vec<Item> = Vec::new();
        let mut buckets: std::collections::BTreeMap<Option<String>, Vec<Item>> =
            groups.keys().cloned().map(|k| (k, Vec::new())).collect();
        for item in items {
            match &item {
                Item::Fn(f)
                    if f.attrs
                        .iter()
                        .any(|a| detect::classify_test_attr(a).is_some()) =>
                {
                    let key = f.attrs.iter().find_map(|a| {
                        let path = detect::as_test_context(a)?;
                        let k = detect::path_to_string(&path);
                        self.test_contexts.plan_for(&k).map(|_| k)
                    });
                    if let Some(bucket) = buckets.get_mut(&key) {
                        bucket.push(item);
                    } else {
                        shared.push(item);
                    }
                }
                _ => shared.push(item),
            }
        }

        let mut new_items = shared;
        for (idx, (key, fns)) in buckets.into_iter().enumerate() {
            if fns.is_empty() {
                continue;
            }
            let child_ident = match &key {
                None => "tests_default".to_owned(),
                Some(k) => format!("tests_with_{}", last_segment_snake(k)),
            };
            let child = self.build_split_child_mod(
                child_ident,
                key.as_deref(),
                runtime,
                runtime_path,
                fns,
                // Per-child cfg-idx index is always 0 (each child
                // suite has only one runtime tuple).
                idx,
            );
            new_items.push(Item::Mod(child));
        }

        // Hoist any inner attrs on the outer mod to outer-style so
        // the rudzio macros expanded inside the children don't
        // re-encounter them as inner attrs (the children carry
        // their own attrs via the suite generation path below).
        hoist_inner_attrs_on_mod(m);
        m.content = Some((brace, new_items));
        let _inserted = self.rewrite.runtimes_used.insert(runtime);
        self.rewrite.changed = true;
        self.warn_span(
            m.ident.span(),
            "module mixed `#[test_context(...)]` and plain tests; split into per-context child modules so each suite tuple has the right `test = ...` path. Suite blocks remain inside the original `#[cfg(test)] mod` for dev-dep visibility.",
        );
    }

    fn build_split_child_mod(
        &mut self,
        ident: String,
        ctx_key: Option<&str>,
        runtime: RuntimeChoice,
        runtime_path: &syn::Path,
        fns: Vec<Item>,
        _idx: usize,
    ) -> ItemMod {
        let plan = ctx_key.and_then(|k| self.test_contexts.plan_for(k));
        let (suite_path, test_path, uses_generated_ctx) = match plan {
            Some(plan) => {
                let base = plan.module_path.as_deref().unwrap_or("crate");
                (
                    parse_path_str(&format!("{base}::{}", plan.suite_ident)),
                    parse_path_str(&format!("{base}::{}", plan.bridge_ident)),
                    true,
                )
            }
            None => (
                parse_path_str("::rudzio::common::context::Suite"),
                parse_path_str("::rudzio::common::context::Test"),
                false,
            ),
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
        let _runtime_used = runtime;
        let mut child_items: Vec<Item> = Vec::with_capacity(fns.len() + 2);
        child_items.push(syn::parse_quote! {
            use super::*;
        });
        if let Some(plan) = plan {
            let base = plan.module_path.as_deref().unwrap_or("crate");
            let path: syn::Path = parse_path_str(&format!("{base}::{}", plan.bridge_ident));
            child_items.push(syn::parse_quote! {
                use #path;
            });
        } else {
            child_items.push(syn::parse_quote! {
                use ::rudzio::common::context::Test;
            });
        }
        let _uses = uses_generated_ctx;
        child_items.extend(fns);
        let ident = syn::Ident::new(&ident, proc_macro2::Span::call_site());
        let mut child: ItemMod = syn::parse_quote! {
            #suite_attr
            mod #ident {}
        };
        child.content = Some((token::Brace::default(), child_items));
        child
    }

    fn try_promote_cfg_test_mod(&mut self, m: &mut ItemMod) {
        // Declaration-only `mod tests;` (no inline body) can't be
        // wrapped with `#[rudzio::suite]` — the macro expects an
        // inline block to descend into.
        if m.content.is_none() {
            return;
        }
        // Only promote modules that actually contain at least one
        // recognized test fn. A `#[cfg(test)]` module with just
        // helper fns (no `#[test]`) must stay a plain `cfg(test)`
        // module; wrapping it with `#[rudzio::suite]` would fail
        // the macro's "at least one #[rudzio::test]" assertion.
        if !module_has_any_test_fn(m) {
            return;
        }
        if has_rudzio_suite(&m.attrs) {
            return;
        }
        // Only promote modules whose OWN attrs include `#[cfg(test)]`.
        // A plain `pub mod outer { #[cfg(test)] mod tests { ... } }`
        // would otherwise trigger: `module_has_any_test_fn(outer)` is
        // true (recursive), so `outer` would get the suite attr even
        // though it isn't test-gated and its non-test items live in
        // the normal lib build. The recursive visit descends into
        // `outer` and catches the inner cfg(test) mod there.
        if !m.attrs.iter().any(is_cfg_test_attr) {
            return;
        }
        // Expand bare `#[cfg(test)]` to `#[cfg(any(test, rudzio_test))]`
        // so the module compiles under BOTH per-crate `cargo test` AND
        // the `cargo rudzio test` aggregator (which depends on the
        // crate as a regular lib, not a `--test` target, and activates
        // `--cfg rudzio_test` via `RUSTFLAGS`). `cfg(test)` stays in
        // the disjunction so dev-dep visibility is unchanged under
        // plain `cargo test`. Compound cfgs such as
        // `#[cfg(all(test, feature = "mock"))]` are untouched — rewriting
        // those is out of v1 scope; user handles them by hand.
        rewrite_cfg_test_to_cfg_any(&mut m.attrs);

        // Pick the runtime: if every recognized test fn in the module
        // forces the same runtime (e.g. all `#[tokio::test(flavor = ...)]`
        // with the same flavor), honor that unanimous choice;
        // otherwise fall back to `--runtime`.
        let runtime = unanimous_inner_runtime(m).unwrap_or(self.default_runtime);
        let runtime_path = parse_path_str(runtime.suite_path());

        // Mixed-context handling: when a single `mod tests` holds
        // both plain `#[tokio::test]` fns and `#[test_context(Ctx)]`
        // fns (or two distinct ctx types), one shared suite attr
        // would type-mismatch half the fns. Split into per-ctx
        // child modules instead.
        let groups = group_test_fns_by_ctx(m, self.test_contexts);
        if groups.len() > 1 {
            self.split_module_by_ctx_groups(m, groups, runtime, &runtime_path);
            return;
        }

        // Choose suite/test paths based on whether any fn inside uses
        // a resolved `#[test_context(T)]`.
        let resolved_ctx = first_resolved_test_context(m, self.test_contexts);
        let (suite_path, test_path, uses_generated_ctx) = match resolved_ctx {
            Some(plan) => {
                let base = plan.module_path.as_deref().unwrap_or("crate");
                if plan.module_path.is_none() {
                    self.warn_span(
                        m.ident.span(),
                        format!(
                            "bridge for `{}` was generated in `{}`, which isn't reachable from `crate::` (typically because it lives under `tests/`). Emitted `crate::{}` as a best-effort placeholder — adjust the `suite = ...` / `test = ...` paths by hand to match your test binary's module tree.",
                            plan.ctx_ident,
                            plan.impl_file.display(),
                            plan.suite_ident,
                        ),
                    );
                }
                (
                    parse_path_str(&format!("{base}::{}", plan.suite_ident)),
                    parse_path_str(&format!("{base}::{}", plan.bridge_ident)),
                    true,
                )
            }
            None => (
                parse_path_str("::rudzio::common::context::Suite"),
                parse_path_str("::rudzio::common::context::Test"),
                false,
            ),
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
        // `#[rudzio::suite(...)]` is an attribute macro on `mod`;
        // its expansion rejects `#![inner]` attrs inside the module
        // body with "an inner attribute is not permitted in this
        // context". Hoist each inner attr to an outer attr on the
        // `mod` item itself — for the common cases
        // (`#![allow(clippy::…)]`, `#![deny(…)]`, `#![expect(…)]`,
        // etc.) this is semantically equivalent: the lints propagate
        // into the body either way.
        hoist_inner_attrs_on_mod(m);
        // Place the suite attribute as the LAST outer attribute on
        // the `mod` item (i.e. immediately before the `mod` keyword)
        // rather than at position 0. Two reasons:
        //   • `#[cfg(test)]` must evaluate before the macro runs.
        //     In a non-test build, cfg(test) prunes the item so the
        //     attribute macro never expands — avoiding references to
        //     rudzio (a dev-dep) from non-test compilations.
        //   • Hoisted `#[expect(...)]` / `#[allow(...)]` attrs need
        //     to sit outside the macro so the lints apply to the
        //     expanded code just like they did in the original body.
        // `push` ends up after any pre-existing outer attrs (cfg,
        // expect, user-written lints) and after the hoisted inner
        // attrs — exactly the ordering the user expects.
        m.attrs.push(suite_attr);
        let _inserted = self.rewrite.runtimes_used.insert(runtime);
        self.rewrite.changed = true;

        // The previous implementation injected
        // `use ::rudzio::common::context::Test;` here so synthesized
        // `_ctx: &Test` parameters resolved. We no longer synthesize
        // that parameter — zero-arg tests are a first-class shape in
        // rudzio, handled by the macro — so the import has no caller.
        let _ = uses_generated_ctx;
    }

    /// Post-pass: a file that's a TEST BINARY ROOT under `tests/`
    /// (i.e. `tests/<stem>.rs` or `tests/<suite>/mod.rs`) becomes an
    /// independent `[[test]] harness = false` binary after the
    /// migration. Cargo needs a `fn main` in such a binary; append
    /// `#[rudzio::main] fn main() {}` if one isn't already there.
    ///
    /// Submodule files deeper in `tests/` are NOT binary roots —
    /// they're pulled in via `mod` declarations from a root file, so
    /// adding a `fn main` to each would be meaningless at best and
    /// produce double linkme registration at worst.
    fn ensure_tests_binary_has_main(&mut self, file: &mut syn::File) {
        if !is_tests_binary_root(&self.file_path) {
            return;
        }
        if !self.rewrite.changed {
            return;
        }
        if file.items.iter().any(item_is_fn_main) {
            return;
        }
        let main_fn: Item = syn::parse_quote! {
            #[::rudzio::main]
            fn main() {}
        };
        file.items.push(main_fn);
    }

    /// Post-pass: pull every converted file-scope `#[rudzio::test]`
    /// fn into a synthesized `#[cfg(test)] #[rudzio::suite([...])] mod
    /// tests { ... }` so rudzio's runner actually
    /// sees them (a bare `#[rudzio::test]` at file scope registers no
    /// tokens and leaves the `Test` type unresolved).
    fn wrap_file_scope_test_fns(&mut self, file: &mut syn::File) {
        let indices: Vec<usize> = file
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                Item::Fn(f) if fn_has_rudzio_test_attr(f) => Some(i),
                _ => None,
            })
            .collect();
        if indices.is_empty() {
            return;
        }
        let first_idx = indices[0];
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
                .unwrap_or(self.default_runtime)
        } else {
            self.default_runtime
        };
        let runtime_path = parse_path_str(runtime.suite_path());
        // If any of the wrapped fns had a resolved `#[test_context(T)]`,
        // their signatures were already rewritten to `&mut CtxBridge`.
        // Pointing the suite attr at `common::Suite` / `common::Test`
        // in that case would produce a type mismatch the moment
        // rudzio's macro tries to resolve `Test<'tc>` against the
        // fn's param type. Use the generated bridge paths instead.
        let (suite_path, test_path, uses_bridge) =
            if let Some(plan) = &self.file_scope_test_context_plan {
                let base = plan.module_path.as_deref().unwrap_or("crate");
                (
                    parse_path_str(&format!("{base}::{}", plan.suite_ident)),
                    parse_path_str(&format!("{base}::{}", plan.bridge_ident)),
                    true,
                )
            } else {
                (
                    parse_path_str("::rudzio::common::context::Suite"),
                    parse_path_str("::rudzio::common::context::Test"),
                    false,
                )
            };

        // The synth module is already `#[cfg(test)]`, so the fns we
        // just pulled in no longer need their own `#[cfg(test)]`
        // attr — strip it to keep the generated code clean.
        let fns: Vec<Item> = fns
            .into_iter()
            .map(|item| match item {
                Item::Fn(mut f) => {
                    f.attrs.retain(|a| !is_cfg_test_attr(a));
                    Item::Fn(f)
                }
                other => other,
            })
            .collect();

        let mut synth_items: Vec<Item> = Vec::with_capacity(fns.len() + 3);
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
            let base = plan.module_path.as_deref().unwrap_or("crate");
            let path: syn::Path = parse_path_str(&format!("{base}::{}", plan.bridge_ident));
            synth_items.push(syn::parse_quote! {
                use #path;
            });
        }
        // Previously we also injected
        // `use ::rudzio::common::context::Test;` here to match the
        // synthesized `_ctx: &Test` parameter. Zero-arg tests are
        // a first-class shape in rudzio now, so the parameter is
        // no longer synthesized and the import has no caller.
        synth_items.extend(fns);
        let _uses_bridge = uses_bridge;

        // `#[allow(non_snake_case)]` on the synth mod silences a
        // rustc warning rudzio's own `#[rudzio::suite]` expansion
        // emits for an internal `__rudzio_run_test_<ModName>_...`
        // function: the module name we pick (camelcase because we
        // want it to be visually distinct as generated) leaks into
        // that function ident and triggers `non_snake_case` in the
        // caller crate. The alternative — naming the mod
        // `tests` lowercase — would collide with
        // any user module already called that; the allow is safer.
        let mut synth: ItemMod = syn::parse_quote! {
            #[cfg(any(test, rudzio_test))]
            #[allow(non_snake_case, reason = "generated by rudzio-migrate")]
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

        let _inserted = self.rewrite.runtimes_used.insert(runtime);
        self.rewrite.changed = true;
    }

    fn try_convert_fn(&mut self, f: &mut ItemFn) {
        let matched_idx_and_kind = f
            .attrs
            .iter()
            .enumerate()
            .find_map(|(i, a)| detect::classify_test_attr(a).map(|d| (i, d)));
        let Some((idx, detected)) = matched_idx_and_kind else {
            return;
        };
        if has_self_receiver(f) {
            self.warn_span(
                f.sig.ident.span(),
                "test fn takes `self` receiver; rudzio tests are free fns — skipping",
            );
            return;
        }
        // Any rstest marker on the fn (or its params) → skip. The
        // rstest model spreads across multiple attribute sites (the
        // outer `#[rstest]` wrapper, inline `#[case(...)]`,
        // `#[values(...)]` on params), and only some of them are
        // visible in `f.attrs`; check both the fn-level attrs and the
        // param-level attrs.
        if f.attrs.iter().any(detect::is_rstest_attr)
            || f.sig.inputs.iter().any(fn_arg_has_rstest_attr)
        {
            self.warn_span(
                f.sig.ident.span(),
                "test fn uses rstest (`#[rstest]` / `#[case]` / `#[values]`); left unchanged — rudzio has no parameterised-test equivalent, rewrite by hand",
            );
            return;
        }
        // Multi-param or non-reference-param test fns are almost
        // certainly `rstest`-style parameterised tests (the `#[case]`
        // or `#[values]` family), which rudzio does not support.
        // Converting them would make rudzio's signature transform
        // paste `<'_, R>` onto things like `&str`, yielding
        // "lifetime and type arguments are not allowed on builtin
        // type `str`". Skip with a warning and let the user rewrite.
        if has_non_ctx_shaped_params(f) {
            self.warn_span(
                f.sig.ident.span(),
                "test fn has parameters that don't look like a single `&T` / `&mut T` context borrow (likely rstest #[case] / #[values]); left unchanged — rudzio has no parameterised-test equivalent, rewrite by hand",
            );
            return;
        }

        for extra in &detected.extra_tokio_args {
            self.warn_span(
                f.sig.ident.span(),
                format!("#[tokio::test] arg `{extra}` dropped; rudzio does not forward it"),
            );
        }
        if let Some(msg) = detected.kind.needs_compat_warning() {
            self.warn_span(f.sig.ident.span(), msg);
        }

        let original_snippet = if self.preserve_originals {
            Some(self.capture_original_snippet(f))
        } else {
            None
        };

        replace_attr_with_rudzio_test(&mut f.attrs, idx);
        let resolved_plan = self.pop_resolved_test_context_plan(&f.attrs);
        let had_resolved_test_context = resolved_plan.is_some();
        let _stripped: bool = self.strip_companion_test_attrs(&mut f.attrs);
        if let Some(plan) = resolved_plan.as_ref() {
            rewrite_ctx_param_to_bridge(f, &plan.ctx_ident, &plan.bridge_ident);
            if self.mod_depth == 0 && self.file_scope_test_context_plan.is_none() {
                self.file_scope_test_context_plan = Some(plan.clone());
            }
        }
        // Capture the return type BEFORE apply_signature_rewrite
        // mutates it — apply_body_rewrite needs to know whether the
        // user already owned a Result body (in which case appending
        // another `Ok(())` produces a dropped-Result-value warning
        // at minimum and an unreachable-statement warning often).
        let original_return = fn_return_kind(&f.sig.output);
        self.apply_signature_rewrite(f, had_resolved_test_context);
        apply_body_rewrite(f, original_return);

        // Pick the runtime: forced by detected kind > file's default.
        let runtime = detected
            .kind
            .forced_runtime()
            .unwrap_or(self.default_runtime);
        let _inserted = self.rewrite.runtimes_used.insert(runtime);
        if self.mod_depth == 0 {
            if let Some(forced) = detected.kind.forced_runtime() {
                let _inserted_file_scope = self.file_scope_runtimes.insert(forced);
            }
        }

        if let Some(snippet) = original_snippet {
            let n = self.rewrite.original_snippets.len();
            self.rewrite.original_snippets.push(snippet);
            // Leading space so prettyplease emits `/// __RUDZIO..._N__`
            // (with a visible gap) rather than `///__RUDZIO..._N__`.
            let sentinel = format!(" __RUDZIO_MIGRATE_ORIGINAL_PLACEHOLDER_{n}__");
            let attr: Attribute = syn::parse_quote! { #[doc = #sentinel] };
            f.attrs.insert(0, attr);
        }

        self.rewrite.changed = true;
        self.report.add_converted(1);
    }

    fn pop_resolved_test_context_plan(&self, attrs: &[Attribute]) -> Option<TestContextPlan> {
        for a in attrs {
            if let Some(path) = detect::as_test_context(a) {
                let key = detect::path_to_string(&path);
                if let Some(plan) = self.test_contexts.plan_for(&key) {
                    return Some(plan.clone());
                }
            }
        }
        None
    }

    fn strip_companion_test_attrs(&mut self, attrs: &mut Vec<Attribute>) -> bool {
        // Two-phase: first classify each attr and record the warnings
        // we'd emit, then retain. This avoids borrowing `self` inside
        // the `retain` closure.
        enum Action {
            Drop(Option<(proc_macro2::Span, String)>),
            Keep,
            DropResolved,
        }
        let mut actions: Vec<Action> = Vec::with_capacity(attrs.len());
        let mut had_resolved_test_context = false;
        for a in attrs.iter() {
            if detect::is_should_panic_attr(a) {
                actions.push(Action::Drop(Some((
                    a.span(),
                    "#[should_panic] stripped; rudzio does not support panic-expectation — rewrite the body to assert the panic manually".to_owned(),
                ))));
                continue;
            }
            if let Some(path) = detect::as_test_context(a) {
                self.stripped_any_test_context_attr = true;
                let key = detect::path_to_string(&path);
                if self.test_contexts.plan_for(&key).is_some() {
                    had_resolved_test_context = true;
                    actions.push(Action::DropResolved);
                } else {
                    actions.push(Action::Drop(Some((
                        a.span(),
                        format!(
                            "#[test_context({key})] stripped without generating a bridge: no `impl AsyncTestContext for {key}` was found in this crate. Finish the migration by hand."
                        ),
                    ))));
                }
                continue;
            }
            actions.push(Action::Keep);
        }
        for action in &actions {
            if let Action::Drop(Some((span, msg))) = action {
                self.warn_span(*span, msg.clone());
            }
        }
        let mut iter = actions.into_iter();
        attrs.retain(|_| matches!(iter.next(), Some(Action::Keep)));
        had_resolved_test_context
    }

    fn apply_signature_rewrite(&mut self, f: &mut ItemFn, had_resolved_test_context: bool) {
        if f.sig.asyncness.is_none() {
            f.sig.asyncness = Some(token::Async(f.sig.fn_token.span));
        }
        if f.sig.inputs.is_empty() {
            // Zero-param tests are a first-class shape in rudzio —
            // the `#[rudzio::test]` macro accepts them as-is and
            // fills in the missing context at expansion time. Don't
            // synthesize `_ctx: &Test`, which would drag a
            // `use ::rudzio::common::context::Test;` into the mod
            // for no user-visible benefit.
        } else if !had_resolved_test_context {
            // Leave user's params alone — custom context path. Warn
            // only when the tool has *no* independent knowledge of
            // what the param should be (a resolved test_context case
            // is a known-good shape).
            self.warn_span(
                f.sig.ident.span(),
                "test fn has a non-trivial parameter list; preserved verbatim — verify the suite's `test = ...` path matches the intended context type",
            );
        }

        match fn_return_kind(&f.sig.output) {
            ReturnKind::UnitImplicit | ReturnKind::UnitExplicit => {
                // Leave as-is. `#[rudzio::test]`'s codegen routes the
                // body through `rudzio::IntoRudzioResult`, which has
                // an impl for `()` — bare-void test bodies work
                // without a signature rewrite. This avoids forcing an
                // `anyhow` dev-dep into migrated crates.
            }
            ReturnKind::Result => {
                // leave as-is
            }
            ReturnKind::Other => {
                self.warn_span(
                    f.sig.ident.span(),
                    "test fn returned a non-Result, non-unit type; wrapping in `Result<(), ::rudzio::BoxError>` and discarding the return value",
                );
                let inner: syn::Block = f.block.as_ref().clone();
                let new_block: syn::Block = syn::parse_quote! {{
                    let _unused = { #inner };
                    ::core::result::Result::Ok(())
                }};
                *f.block = new_block;
                f.sig.output =
                    syn::parse_quote! { -> ::core::result::Result<(), ::rudzio::BoxError> };
            }
        }
    }

    fn capture_original_snippet(&self, f: &ItemFn) -> String {
        let span_start = f
            .attrs
            .iter()
            .map(|a| a.span().byte_range().start)
            .min()
            .unwrap_or_else(|| f.sig.fn_token.span.byte_range().start);
        let span_end = f.block.span().byte_range().end;
        let src: &str = &self.source;
        let len = src.len();
        if span_start < len && span_end <= len && span_start <= span_end {
            src[span_start..span_end].to_owned()
        } else {
            String::new()
        }
    }

    fn warn_span(&mut self, span: proc_macro2::Span, message: impl Into<String>) {
        let range = span.byte_range();
        let offset = range.start;
        let len = range.end.saturating_sub(range.start);
        self.report.warn_with_span(
            self.file_path.clone(),
            span.start().line,
            offset,
            len,
            Arc::clone(&self.source),
            message,
        );
    }
}

fn apply_body_rewrite(f: &mut ItemFn, original_return: ReturnKind) {
    // Only the `Other` case (non-Result, non-unit) needs a body
    // tweak — `apply_signature_rewrite` already replaced the body
    // there with `{ let _unused = { <original> }; Ok(()) }`. Result
    // and unit-return bodies are left verbatim: the `rudzio::test`
    // codegen routes them through `IntoRudzioResult` and handles
    // each shape without a source-level wrapper.
    let _ = (f, original_return);
}

fn ends_with_ok(stmts: &[Stmt]) -> bool {
    let Some(last) = stmts.last() else {
        return false;
    };
    let expr = match last {
        Stmt::Expr(e, _) => e,
        _ => return false,
    };
    matches!(
        expr,
        Expr::Call(c)
            if matches!(
                &*c.func,
                Expr::Path(p)
                    if detect::path_to_string(&p.path) == "::core::result::Result::Ok"
                        || detect::path_to_string(&p.path) == "Ok"
            )
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnKind {
    UnitImplicit,
    UnitExplicit,
    Result,
    Other,
}

fn fn_return_kind(ret: &ReturnType) -> ReturnKind {
    match ret {
        ReturnType::Default => ReturnKind::UnitImplicit,
        ReturnType::Type(_, ty) => match &**ty {
            Type::Tuple(t) if t.elems.is_empty() => ReturnKind::UnitExplicit,
            Type::Path(p) => {
                let last = p
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default();
                if last == "Result" {
                    ReturnKind::Result
                } else {
                    ReturnKind::Other
                }
            }
            _ => ReturnKind::Other,
        },
    }
}

fn replace_attr_with_rudzio_test(attrs: &mut Vec<Attribute>, idx: usize) {
    let new_attr: Attribute = syn::parse_quote! { #[::rudzio::test] };
    attrs[idx] = new_attr;
}

fn fn_arg_has_rstest_attr(arg: &FnArg) -> bool {
    match arg {
        FnArg::Typed(pt) => pt.attrs.iter().any(detect::is_rstest_attr),
        FnArg::Receiver(r) => r.attrs.iter().any(detect::is_rstest_attr),
    }
}

fn has_self_receiver(f: &ItemFn) -> bool {
    f.sig
        .inputs
        .iter()
        .any(|arg| matches!(arg, FnArg::Receiver(_)))
}

/// True if the fn has params that don't match the "single ctx borrow"
/// shape rudzio expects: exactly zero params (we inject `_ctx: &Test`)
/// or exactly one param that's a `&T` / `&mut T` reference (user's own
/// ctx type, left alone — rudzio::test's macro transform will fix the
/// generics). Anything else — multiple params, owned values, tuples —
/// signals a parameterised-test library (rstest, etc.) whose shape the
/// rudzio macro can't handle.
fn has_non_ctx_shaped_params(f: &ItemFn) -> bool {
    let n = f.sig.inputs.len();
    if n == 0 {
        return false;
    }
    if n > 1 {
        return true;
    }
    let Some(arg) = f.sig.inputs.first() else {
        return false;
    };
    let FnArg::Typed(pat_type) = arg else {
        return true;
    };
    !matches!(&*pat_type.ty, Type::Reference(_))
}

fn is_cfg_test_attr(attr: &Attribute) -> bool {
    if detect::path_to_string(attr.path()) != "cfg" {
        return false;
    }
    let Meta::List(list) = &attr.meta else {
        return false;
    };
    list.tokens.to_string().trim() == "test"
}

/// Rewrite every bare `#[cfg(test)]` in `attrs` to
/// `#[cfg(any(test, rudzio_test))]`. Idempotent: compound cfgs
/// (including an already-rewritten `any(test, rudzio_test)`) do not
/// match `is_cfg_test_attr`, so running this twice is a no-op.
fn rewrite_cfg_test_to_cfg_any(attrs: &mut Vec<Attribute>) {
    for attr in attrs.iter_mut() {
        if is_cfg_test_attr(attr) {
            *attr = syn::parse_quote!(#[cfg(any(test, rudzio_test))]);
        }
    }
}

/// File-wide pass that rewrites every bare `#[cfg_attr(test, ...)]`
/// to `#[cfg_attr(any(test, rudzio_test), ...)]`. Motivation: a struct
/// in `src/**` carrying `#[cfg_attr(test, derive(fake::Dummy))]` is
/// used by `#[cfg(any(test, rudzio_test))] mod tests` in the same or
/// another file of the same crate. Under `cargo rudzio test` the
/// aggregator compiles the crate as a plain lib with `--cfg
/// rudzio_test` (no `cfg(test)`), so the derive doesn't fire and the
/// test fails to typecheck. Broadening the predicate makes the
/// conditional attr active under both `cargo test` and
/// `cargo rudzio test`.
///
/// Only the bare `cfg_attr(test, ...)` shape is rewritten. Compound
/// predicates (`all(test, ...)`, `any(test, ...)`, feature gates) are
/// left alone — same v1-scope policy as `rewrite_cfg_test_to_cfg_any`.
/// Returns the number of attributes rewritten so the caller can mark
/// the file as changed.
fn rewrite_cfg_attr_test_in_file(file: &mut syn::File) -> usize {
    let mut rw = CfgAttrTestRewriter { rewrites: 0 };
    rw.visit_file_mut(file);
    rw.rewrites
}

struct CfgAttrTestRewriter {
    rewrites: usize,
}

impl VisitMut for CfgAttrTestRewriter {
    fn visit_attribute_mut(&mut self, attr: &mut Attribute) {
        if rewrite_cfg_attr_test_attr(attr) {
            self.rewrites += 1;
        }
        visit_mut::visit_attribute_mut(self, attr);
    }
}

fn rewrite_cfg_attr_test_attr(attr: &mut Attribute) -> bool {
    if !attr.path().is_ident("cfg_attr") {
        return false;
    }
    let Meta::List(list) = &attr.meta else {
        return false;
    };
    let metas: Punctuated<Meta, syn::Token![,]> =
        match list.parse_args_with(Punctuated::<Meta, syn::Token![,]>::parse_terminated) {
            Ok(m) => m,
            Err(_) => return false,
        };
    let first_is_bare_test = matches!(
        metas.first(),
        Some(Meta::Path(p)) if p.is_ident("test")
    );
    if !first_is_bare_test {
        return false;
    }
    let rest: Vec<&Meta> = metas.iter().skip(1).collect();
    let rebuilt: Attribute = if rest.is_empty() {
        // `#[cfg_attr(test)]` — degenerate; still rewrite predicate.
        syn::parse_quote!(#[cfg_attr(any(test, rudzio_test))])
    } else {
        syn::parse_quote!(#[cfg_attr(any(test, rudzio_test), #(#rest),*)])
    };
    *attr = rebuilt;
    true
}

fn has_rudzio_suite(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        detect::path_to_string(a.path()) == "rudzio::suite"
            || detect::path_to_string(a.path()) == "::rudzio::suite"
    })
}

fn unanimous_inner_runtime(m: &ItemMod) -> Option<RuntimeChoice> {
    let Some((_, items)) = &m.content else {
        return None;
    };
    let mut runtimes: BTreeSet<RuntimeChoice> = BTreeSet::new();
    collect_runtime_hints(items, &mut runtimes);
    if runtimes.len() == 1 {
        runtimes.into_iter().next()
    } else {
        None
    }
}

fn collect_runtime_hints(items: &[Item], out: &mut BTreeSet<RuntimeChoice>) {
    for item in items {
        match item {
            Item::Fn(f) => {
                for attr in &f.attrs {
                    if let Some(d) = detect::classify_test_attr(attr) {
                        if let Some(rt) = d.kind.forced_runtime() {
                            let _inserted = out.insert(rt);
                        }
                    }
                }
            }
            Item::Mod(sub) => {
                if let Some((_, inner)) = &sub.content {
                    collect_runtime_hints(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// Group every recognised test fn inside `m` by its target context
/// (None for plain tests, Some(ctx_key) for resolved `#[test_context]`
/// uses). The map's key set is what `try_promote_cfg_test_mod` uses
/// to decide single-suite vs split — len == 1 is the single case,
/// len > 1 triggers the per-ctx split.
/// Convert `crate::foo::DbCtx` → `db_ctx`. Used to derive a snake-case
/// suffix for split-child module names (`tests_with_db_ctx`).
fn last_segment_snake(path: &str) -> String {
    let last = path.rsplit("::").next().unwrap_or(path);
    let mut out = String::with_capacity(last.len() + 4);
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
    out
}

fn group_test_fns_by_ctx(
    m: &ItemMod,
    resolver: &TestContextResolver,
) -> std::collections::BTreeMap<Option<String>, ()> {
    let mut groups: std::collections::BTreeMap<Option<String>, ()> =
        std::collections::BTreeMap::new();
    let Some((_, items)) = &m.content else {
        return groups;
    };
    for item in items {
        if let Item::Fn(f) = item {
            if !f
                .attrs
                .iter()
                .any(|a| detect::classify_test_attr(a).is_some())
            {
                continue;
            }
            let key = f.attrs.iter().find_map(|a| {
                let path = detect::as_test_context(a)?;
                let k = detect::path_to_string(&path);
                resolver.plan_for(&k).map(|_| k)
            });
            let _present = groups.insert(key, ());
        }
    }
    groups
}

fn first_resolved_test_context<'r>(
    m: &ItemMod,
    resolver: &'r TestContextResolver,
) -> Option<&'r TestContextPlan> {
    let Some((_, items)) = &m.content else {
        return None;
    };
    for item in items {
        if let Item::Fn(f) = item {
            for attr in &f.attrs {
                if let Some(path) = detect::as_test_context(attr) {
                    let key = detect::path_to_string(&path);
                    if let Some(plan) = resolver.plan_for(&key) {
                        return Some(plan);
                    }
                }
            }
        }
    }
    None
}

fn module_has_any_test_fn(m: &ItemMod) -> bool {
    let Some((_, items)) = &m.content else {
        return false;
    };
    items.iter().any(item_has_any_test_fn)
}

fn item_has_any_test_fn(item: &Item) -> bool {
    match item {
        Item::Fn(f) => f
            .attrs
            .iter()
            .any(|a| detect::classify_test_attr(a).is_some()),
        Item::Mod(m) => module_has_any_test_fn(m),
        _ => false,
    }
}

fn parse_path_str(s: &str) -> syn::Path {
    syn::parse_str::<syn::Path>(s).expect("static path string")
}

fn rewrite_ctx_param_to_bridge(f: &mut ItemFn, ctx_ident: &str, bridge_ident: &str) {
    let Some(first) = f.sig.inputs.first_mut() else {
        return;
    };
    let FnArg::Typed(pat_type) = first else {
        return;
    };
    let Type::Reference(type_ref) = &mut *pat_type.ty else {
        return;
    };
    let Type::Path(type_path) = &mut *type_ref.elem else {
        return;
    };
    let Some(last) = type_path.path.segments.last_mut() else {
        return;
    };
    if last.ident == ctx_ident {
        last.ident = syn::Ident::new(bridge_ident, last.ident.span());
    }
}

fn is_tests_binary_root(path: &std::path::Path) -> bool {
    // Only the binary roots get `fn main` synthesised. Two shapes
    // count: a direct child `tests/<stem>.rs`, or a suite-dir
    // `tests/<suite>/mod.rs`. Anything deeper is a submodule and
    // inherits its main from the root via `mod` declarations.
    let mut components: Vec<&std::ffi::OsStr> = path.components().map(|c| c.as_os_str()).collect();
    // Find the outermost `tests` segment.
    let Some(tests_idx) = components
        .iter()
        .position(|c| *c == std::ffi::OsStr::new("tests"))
    else {
        return false;
    };
    let rel = components.split_off(tests_idx + 1);
    // `tests/<stem>.rs`: exactly one `.rs` component after `tests/`.
    if rel.len() == 1 {
        return rel[0].to_str().is_some_and(|s| s.ends_with(".rs"));
    }
    // `tests/<suite>/mod.rs`: exactly two components, last is `mod.rs`.
    rel.len() == 2 && rel[1] == std::ffi::OsStr::new("mod.rs")
}

fn item_is_fn_main(item: &Item) -> bool {
    if let Item::Fn(f) = item {
        f.sig.ident == "main"
    } else {
        false
    }
}

fn fn_has_rudzio_test_attr(f: &ItemFn) -> bool {
    f.attrs.iter().any(|a| {
        let p = detect::path_to_string(a.path());
        p == "::rudzio::test" || p == "rudzio::test"
    })
}

fn file_has_rudzio_test_at_top_level(file: &syn::File) -> bool {
    file.items.iter().any(|item| {
        if let Item::Fn(f) = item {
            f.attrs
                .iter()
                .any(|a| detect::path_to_string(a.path()) == "::rudzio::test")
        } else {
            false
        }
    })
}

fn ensure_file_scope_test_import(file: &mut syn::File) {
    let already = file.items.iter().any(|item| {
        matches!(
            item,
            Item::Use(u)
                if quote::ToTokens::to_token_stream(&u.tree)
                    .to_string()
                    .contains("Test")
        )
    });
    if already {
        return;
    }
    let use_item: Item = syn::parse_quote! {
        use ::rudzio::common::context::Test;
    };
    file.items.insert(0, use_item);
}

/// Removes `use test_context::test_context;` imports inside the module
/// once all `#[test_context(...)]` attributes have been stripped — they
/// were what referenced the attribute macro. Leaves other `test_context`
/// re-exports (e.g. the trait `AsyncTestContext` itself) alone.
fn prune_unused_test_context_import(m: &mut ItemMod) {
    let Some((_, items)) = &mut m.content else {
        return;
    };
    items.retain(|item| {
        let Item::Use(u) = item else {
            return true;
        };
        let tokens = quote::ToTokens::to_token_stream(&u.tree).to_string();
        // Canonical form prettyplease emits for `use test_context::test_context;`.
        let normalized: String = tokens.split_whitespace().collect::<Vec<_>>().join("");
        !(normalized == "test_context::test_context"
            || normalized == "::test_context::test_context")
    });
}

/// After we strip every `#[test_context(...)]` attribute, the
/// `use test_context::test_context;` (or grouped equivalent) the
/// user's tests imported is dead. Walk every `use test_context::...`
/// item and drop any leaf named `test_context` — leaving the trait
/// (`AsyncTestContext`, `TestContext`) imports intact.
fn prune_test_context_macro_imports(file: &mut syn::File) {
    prune_test_context_macro_imports_in_items(&mut file.items);
}

fn prune_test_context_macro_imports_in_items(items: &mut Vec<Item>) {
    items.retain_mut(|item| {
        let Item::Use(u) = item else {
            return true;
        };
        let root_test_context_match =
            matches!(&u.tree, syn::UseTree::Path(p) if p.ident == "test_context");
        if !root_test_context_match {
            return true;
        }
        if let syn::UseTree::Path(p) = &mut u.tree {
            let _empty = drop_test_context_leaf(&mut p.tree);
        }
        if tree_is_empty(&u.tree) {
            return false;
        }
        true
    });
}

/// Recursively remove a `test_context` leaf or grouped item from a
/// `UseTree` rooted under `test_context::`. Returns `true` if the
/// caller's containing tree is now empty (e.g. an empty Group).
fn drop_test_context_leaf(tree: &mut syn::UseTree) -> bool {
    use syn::UseTree;
    match tree {
        UseTree::Name(n) if n.ident == "test_context" => true,
        UseTree::Rename(r) if r.ident == "test_context" => true,
        UseTree::Group(g) => {
            // `Punctuated` doesn't expose `retain_mut`; rebuild via
            // a Vec round-trip. The Group's items get reassembled
            // with the same comma punctuation.
            let kept: Vec<UseTree> = std::mem::take(&mut g.items)
                .into_iter()
                .filter_map(|mut inner| {
                    if drop_test_context_leaf(&mut inner) {
                        None
                    } else {
                        Some(inner)
                    }
                })
                .collect();
            for inner in kept {
                g.items.push(inner);
            }
            g.items.is_empty()
        }
        UseTree::Path(p) => drop_test_context_leaf(&mut p.tree),
        _ => false,
    }
}

fn tree_is_empty(tree: &syn::UseTree) -> bool {
    use syn::UseTree;
    match tree {
        UseTree::Path(p) => tree_is_empty(&p.tree),
        UseTree::Group(g) => g.items.is_empty(),
        _ => false,
    }
}

fn hoist_inner_attrs_on_mod(m: &mut ItemMod) {
    for attr in &mut m.attrs {
        if matches!(attr.style, syn::AttrStyle::Inner(_)) {
            attr.style = syn::AttrStyle::Outer;
        }
    }
}

/// Emitted verbatim by `emit::unparse_with_originals` whenever we need
/// to format a `Punctuated` list of attrs onto an item — kept here so
/// the rewrite module owns all path-synthesis.
#[allow(dead_code)]
fn debug_assert_attr_list(_attrs: &Punctuated<TokenStream, token::Comma>) {}

#[allow(dead_code)]
fn placeholder_for_block(_b: &ExprBlock) {}
