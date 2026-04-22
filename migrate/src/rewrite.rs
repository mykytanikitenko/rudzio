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
use syn::{
    Attribute, Expr, ExprBlock, FnArg, Item, ItemFn, ItemMod, Meta, ReturnType, Stmt, Type,
};

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
    };
    walker.visit_file_mut(file);
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
    }

    fn visit_item_mod_mut(&mut self, m: &mut ItemMod) {
        self.try_promote_cfg_test_mod(m);
        self.mod_depth = self.mod_depth.saturating_add(1);
        visit_mut::visit_item_mod_mut(self, m);
        self.mod_depth = self.mod_depth.saturating_sub(1);
        prune_unused_test_context_import(m);
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
        // IMPORTANT: keep the `#[cfg(test)]` attr in place. Stripping
        // it would compile the module unconditionally, losing access
        // to dev-dependencies (`pretty_assertions`, `mockall`, etc.)
        // and likely breaking builds. The rudzio runner runs as a
        // `[[test]] harness = false` binary — i.e. a test target —
        // so `#[cfg(test)]` is active there and the `linkme` entries
        // still register. `#[cfg(all(test, feature = "mock"))]` and
        // similar compound cfgs are left intact for the same reason.
        let _kept_cfg_test = m.attrs.iter().any(is_cfg_test_attr);

        // Pick the runtime: if every recognized test fn in the module
        // forces the same runtime (e.g. all `#[tokio::test(flavor = ...)]`
        // with the same flavor), honor that unanimous choice;
        // otherwise fall back to `--runtime`.
        let runtime = unanimous_inner_runtime(m).unwrap_or(self.default_runtime);
        let runtime_path = parse_path_str(runtime.suite_path());

        // Choose suite/test paths based on whether any fn inside uses
        // a resolved `#[test_context(T)]`.
        let resolved_ctx = first_resolved_test_context(m, self.test_contexts);
        let (suite_path, test_path, uses_generated_ctx) = match resolved_ctx {
            Some(plan) => (
                parse_path_str(&format!("crate::{}", plan.suite_ident)),
                parse_path_str(&format!("crate::{}", plan.bridge_ident)),
                true,
            ),
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
        m.attrs.insert(0, suite_attr);
        let _inserted = self.rewrite.runtimes_used.insert(runtime);
        self.rewrite.changed = true;

        if !uses_generated_ctx {
            // For the default common::context path, inject a Test
            // import so test fn signatures can use the bare name.
            ensure_test_import(m);
        }
    }

    /// Post-pass: pull every converted file-scope `#[rudzio::test]`
    /// fn into a synthesized `#[cfg(test)] #[rudzio::suite([...])] mod
    /// __rudzio_migrated_tests { ... }` so rudzio's runner actually
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
        let suite_path = parse_path_str("::rudzio::common::context::Suite");
        let test_path = parse_path_str("::rudzio::common::context::Test");

        let mut synth_items: Vec<Item> = Vec::with_capacity(fns.len() + 2);
        synth_items.push(syn::parse_quote! {
            use ::rudzio::common::context::Test;
        });
        synth_items.push(syn::parse_quote! {
            use super::*;
        });
        synth_items.extend(fns);

        let mut synth: ItemMod = syn::parse_quote! {
            #[cfg(test)]
            #[::rudzio::suite([
                (
                    runtime = #runtime_path,
                    suite = #suite_path,
                    test = #test_path,
                ),
            ])]
            mod __rudzio_migrated_tests {}
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
        }
        self.apply_signature_rewrite(f, had_resolved_test_context);
        apply_body_rewrite(f);
        if matches!(fn_return_kind(&f.sig.output), ReturnKind::UnitImplicit | ReturnKind::UnitExplicit) {
            // Now `apply_signature_rewrite` has already upgraded the return
            // type; mark `anyhow` needed.
        }
        self.rewrite.needs_anyhow = true;

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

    fn pop_resolved_test_context_plan(
        &self,
        attrs: &[Attribute],
    ) -> Option<TestContextPlan> {
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
            // The rudzio::suite macro resolves a bare `Test` at expansion
            // time using the `test = ...` path in the suite tuple. We
            // emit `&Test` here and rely on a module-scope `use` that
            // the module-promotion step injects.
            let arg: FnArg = syn::parse_quote! { _ctx: &Test };
            f.sig.inputs.push(arg);
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
                f.sig.output = syn::parse_quote! { -> ::anyhow::Result<()> };
            }
            ReturnKind::Result => {
                // leave as-is
            }
            ReturnKind::Other => {
                self.warn_span(
                    f.sig.ident.span(),
                    "test fn returned a non-Result type; wrapping in ::anyhow::Result<()> and discarding the return value",
                );
                let inner: syn::Block = f.block.as_ref().clone();
                let new_block: syn::Block = syn::parse_quote! {{
                    let _unused = { #inner };
                    ::core::result::Result::Ok(())
                }};
                *f.block = new_block;
                f.sig.output = syn::parse_quote! { -> ::anyhow::Result<()> };
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

fn apply_body_rewrite(f: &mut ItemFn) {
    let block = &mut f.block;
    let needs_ok = !ends_with_ok(&block.stmts);
    if needs_ok {
        let ok_expr: Expr = syn::parse_quote! { ::core::result::Result::Ok(()) };
        block.stmts.push(Stmt::Expr(ok_expr, None));
    }
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

fn has_rudzio_suite(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| detect::path_to_string(a.path()) == "rudzio::suite" || detect::path_to_string(a.path()) == "::rudzio::suite")
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
        Item::Fn(f) => f.attrs.iter().any(|a| detect::classify_test_attr(a).is_some()),
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

fn ensure_test_import(m: &mut ItemMod) {
    let Some((_brace, items)) = &mut m.content else {
        return;
    };
    let already_imported = items.iter().any(|it| {
        let Item::Use(u) = it else { return false };
        brings_bare_test_name_into_scope(&u.tree)
    });
    if already_imported {
        return;
    }
    let use_item: Item = syn::parse_quote! {
        use ::rudzio::common::context::Test;
    };
    items.insert(0, use_item);
}

/// True iff the `use` tree binds the bare name `Test` (as a terminal
/// name or aliased as `Test`) — e.g. `use foo::Test;`,
/// `use foo::X as Test;`, or `use foo::{A, Test, B};`. Glob imports
/// and unrelated names containing "Test" as a substring do not count.
fn brings_bare_test_name_into_scope(tree: &syn::UseTree) -> bool {
    use syn::UseTree;
    match tree {
        UseTree::Name(n) => n.ident == "Test",
        UseTree::Rename(r) => r.rename == "Test",
        UseTree::Path(p) => brings_bare_test_name_into_scope(&p.tree),
        UseTree::Group(g) => g.items.iter().any(brings_bare_test_name_into_scope),
        UseTree::Glob(_) => false,
    }
}

/// Emitted verbatim by `emit::unparse_with_originals` whenever we need
/// to format a `Punctuated` list of attrs onto an item — kept here so
/// the rewrite module owns all path-synthesis.
#[allow(dead_code)]
fn debug_assert_attr_list(_attrs: &Punctuated<TokenStream, token::Comma>) {}

#[allow(dead_code)]
fn placeholder_for_block(_b: &ExprBlock) {}
