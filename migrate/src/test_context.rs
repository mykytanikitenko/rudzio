//! `#[test_context(T)]` migration.
//!
//! The `test-context` crate's shape maps cleanly onto rudzio's `Suite` /
//! `Test` split, so we generate bridge impls and rewire the enclosing
//! `#[rudzio::suite(...)]` attribute to point at them.
//!
//! High-level flow:
//!   1. `resolve(packages)` pre-scans every file in every package to
//!      find `#[test_context(Ctx)]` attribute uses and
//!      `impl AsyncTestContext for Ctx` (or sync `TestContext`)
//!      blocks.
//!   2. For each ctx type that has both a use-site and a local impl,
//!      we plan a migration with the generated suite-struct name and
//!      the bridge `impl` text.
//!   3. Use-sites without a resolved impl fall through to the
//!      graceful-degradation path (warn, strip `#[test_context]`,
//!      leave the fn otherwise alone).
//!
//! The rewriter consults the resolver to decide:
//!   a. when wrapping a module, which suite / test path to emit in
//!      `#[rudzio::suite([...])]` (the generated `XxxRudzioSuite` for
//!      modules whose fns use `#[test_context(Xxx)]`, or the default
//!      `common::context::Suite/Test` otherwise);
//!   b. when processing the impl-file, whether to append the generated
//!      bridge code.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use syn::visit::{self, Visit};
use syn::{ImplItem, Type, Visibility};

use crate::detect::{as_test_context, path_to_string};
use crate::discovery::Package;

/// Plan for a single resolved `#[test_context(Ctx)]` use-site.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Plan {
    /// Generated bridge wrapper type ident (e.g. `MyCtxRudzioBridge`).
    /// Wraps `ctx_ident` and adds the `<'test_context, R>` generics that
    /// rudzio's `#[rudzio::test]` macro requires.
    pub bridge_ident: String,
    /// Local type identifier for the ctx (e.g. `MyCtx`). Used in the
    /// emitted bridge code.
    pub ctx_ident: String,
    /// String form of the ctx type path as referenced by
    /// `#[test_context(T)]`. Used as the resolver's map key.
    pub ctx_key: String,
    /// Token text of the user's `pub`/`pub(crate)`/(private)
    /// visibility on the ctx struct. The bridge struct + suite
    /// struct mirror this so the generated `pub struct Bridge {
    /// pub inner: Ctx }` doesn't run into `private_interfaces`
    /// when `Ctx` itself is `pub(crate)` or private.
    pub ctx_visibility: String,
    /// Path to the .rs file containing `impl AsyncTestContext for T`.
    /// The generated bridge impls are appended to this file.
    pub impl_file: PathBuf,
    /// `true` if the resolved trait is `AsyncTestContext`; `false` for
    /// sync `TestContext`.
    pub is_async: bool,
    /// Best-effort module path from the test binary's crate root to
    /// the module that owns `impl_file`. `Some("crate::foo::bar")`
    /// when the impl lives under `src/` in a resolvable spot;
    /// `Some("crate")` when it's at the lib root; `None` when it
    /// lives under `tests/` (where the binary's module tree isn't
    /// knowable from file paths alone) or in a location the
    /// resolver can't map. `None` triggers a warning at emission
    /// time and falls back to `crate::<Ident>`, which the user
    /// will typically need to adjust.
    pub module_path: Option<String>,
    /// Generated bridge suite struct ident, e.g. `MyCtxRudzioSuite`.
    pub suite_ident: String,
}

/// State the rewriter consults to decide how to emit a `#[rudzio::suite]`
/// attribute and whether to append generated bridge impls to a file.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Resolver {
    /// Resolved migrations, keyed on the ctx path string.
    pub plans: BTreeMap<String, Plan>,
    /// `#[test_context(T)]` uses that couldn't be matched to a local
    /// `impl AsyncTestContext for T` — the rewriter emits a warning
    /// for each and strips the attr without generating any bridge.
    pub unresolved: HashSet<String>,
}

/// Visitor that walks one source file and records ctx structs, ctx
/// impls, and `#[test_context(...)]` use-sites into the surrounding
/// per-package collections.
struct Scanner<'src> {
    /// Per-ident visibility of every `struct Ctx { ... }` we saw.
    ctx_visibility: &'src mut BTreeMap<String, Visibility>,
    /// Path of the file currently being scanned (used to record the
    /// owning file for an `impl` we accept).
    current_file: &'src Path,
    /// Map from ctx-type-key to `(impl-file, is_async)`.
    impls: &'src mut BTreeMap<String, (PathBuf, bool)>,
    /// Set of ctx-type-keys observed on `#[test_context(...)]`.
    use_sites: &'src mut BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    #[inline]
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        for attr in &i.attrs {
            if let Some(path) = as_test_context(attr) {
                let key = path_to_string(&path);
                let _inserted = self.use_sites.insert(key);
            }
        }
        visit::visit_item_fn(self, i);
    }

    #[inline]
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let Some((_bang, trait_path, _for_token)) = &i.trait_ else {
            visit::visit_item_impl(self, i);
            return;
        };
        let trait_name = last_segment(&path_to_string(trait_path));
        let is_async = match trait_name.as_str() {
            "AsyncTestContext" => true,
            "TestContext" => false,
            _ => {
                visit::visit_item_impl(self, i);
                return;
            }
        };
        let Type::Path(ty_path) = &*i.self_ty else {
            return;
        };
        let ty_key = path_to_string(&ty_path.path);
        let mut seen_setup = false;
        let mut seen_teardown = false;
        for impl_item in &i.items {
            if let ImplItem::Fn(method) = impl_item {
                match method.sig.ident.to_string().as_str() {
                    "setup" => seen_setup = true,
                    "teardown" => seen_teardown = true,
                    _ => {}
                }
            }
        }
        if seen_setup && seen_teardown {
            let _prev = self
                .impls
                .insert(ty_key, (self.current_file.to_path_buf(), is_async));
        }
    }

    #[inline]
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let _prev = self
            .ctx_visibility
            .insert(i.ident.to_string(), i.vis.clone());
        visit::visit_item_struct(self, i);
    }
}

impl Resolver {
    /// Construct an empty resolver.
    #[inline]
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// True if `ctx_key` was seen but no local impl could be matched.
    #[inline]
    #[must_use]
    pub fn is_unresolved(&self, ctx_key: &str) -> bool {
        self.unresolved.contains(ctx_key)
    }

    /// Look up the plan for `ctx_key`, if any.
    #[inline]
    #[must_use]
    pub fn plan_for(&self, ctx_key: &str) -> Option<&Plan> {
        self.plans.get(ctx_key)
    }
}

/// Pre-scan every file in every package and collect ctx use-sites and
/// matching `impl (Async)?TestContext for T` blocks.
///
/// # Errors
///
/// Returns the underlying I/O error if a source file can't be read.
#[inline]
pub fn resolve(packages: &[Package]) -> Result<Resolver> {
    let mut resolver = Resolver::empty();
    for pkg in packages {
        resolve_package(pkg, &mut resolver)?;
    }
    Ok(resolver)
}

/// Render the bridge `Suite` + `Test` impls for a resolved ctx.
///
/// Appended to the end of the impl file by the caller.
#[inline]
#[must_use]
pub fn render_bridge_impls(plan: &Plan) -> String {
    let ctx = &plan.ctx_ident;
    let bridge = &plan.bridge_ident;
    let suite = &plan.suite_ident;
    // Mirror the user's visibility on Ctx; `pub Bridge { pub inner:
    // Ctx }` would otherwise trip `private_interfaces` whenever the
    // user's Ctx is `pub(crate)` or private. The empty-string case
    // (private struct) renders as no visibility marker.
    let vis = if plan.ctx_visibility.is_empty() {
        String::new()
    } else {
        format!("{} ", plan.ctx_visibility)
    };
    let setup_call = if plan.is_async {
        format!("<{ctx} as ::test_context::AsyncTestContext>::setup().await")
    } else {
        format!("<{ctx} as ::test_context::TestContext>::setup()")
    };
    let teardown_call = if plan.is_async {
        format!("<{ctx} as ::test_context::AsyncTestContext>::teardown(self.inner).await;")
    } else {
        format!("<{ctx} as ::test_context::TestContext>::teardown(self.inner);")
    };
    let bridge_block = render_bridge_block(ctx, bridge, &vis);
    let suite_block = render_suite_block(bridge, suite, &vis, &setup_call, &teardown_call);
    format!("\n{bridge_block}{suite_block}")
}

/// Last `::`-delimited segment of `path_str`.
fn last_segment(path_str: &str) -> String {
    path_str.rsplit("::").next().unwrap_or(path_str).to_owned()
}

/// Compute `Some("crate::foo::bar")` when `impl_file` maps to a
/// reachable position in the lib's module tree.
///
/// Returns `None` for impl files under `tests/` or anywhere else
/// (e.g. `examples/`, `benches/`) where the test binary's module tree
/// isn't derivable from the path alone and the user needs to tell us
/// via a custom `mod` declaration.
fn infer_module_path(impl_file: &Path, pkg_root: &Path) -> Option<String> {
    let rel = impl_file.strip_prefix(pkg_root).ok()?;
    let components: Vec<&str> = rel
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    let (first, rest) = components.split_first()?;
    if *first != "src" {
        return None;
    }
    match rest {
        [] => None,
        ["lib.rs" | "main.rs"] => Some("crate".to_owned()),
        [single] => single
            .strip_suffix(".rs")
            .map(|stem| format!("crate::{stem}")),
        deeper => render_deeper_module_path(deeper),
    }
}

/// Convert a `pub`/`pub(crate)`/empty `Visibility` token tree into the
/// trimmed source string the bridge renderer wants. Empty visibility
/// (private struct) returns `pub(crate)` so the generated wrapper isn't
/// less private than the inner field — except when the user's struct is
/// genuinely private, in which case the caller passes through whatever
/// the scanner saw.
fn render_visibility(vis: Option<&Visibility>) -> String {
    vis.map(|value| {
        quote::ToTokens::to_token_stream(value)
            .to_string()
            .trim()
            .to_owned()
    })
    .filter(|text| !text.is_empty())
    .unwrap_or_else(|| "pub(crate)".to_owned())
}

/// Render `crate::a::b::c` for a module whose impl file lies deeper
/// than the lib root (the components after `src/`). Returns `None` when
/// the leaf doesn't end in `.rs`.
fn render_deeper_module_path(components: &[&str]) -> Option<String> {
    let mut segments: Vec<String> = Vec::new();
    let last_idx = components.len().saturating_sub(1);
    for (idx, comp) in components.iter().enumerate() {
        if idx == last_idx {
            if *comp == "mod.rs" {
                break;
            }
            let stem = comp.strip_suffix(".rs")?;
            segments.push(stem.to_owned());
        } else {
            segments.push((*comp).to_owned());
        }
    }
    if segments.is_empty() {
        Some("crate".to_owned())
    } else {
        Some(format!("crate::{}", segments.join("::")))
    }
}

/// Per-package pass: walk every src + test file, populate `resolver`.
fn resolve_package(pkg: &Package, resolver: &mut Resolver) -> Result<()> {
    let mut use_sites: BTreeSet<String> = BTreeSet::new();
    let mut impls: BTreeMap<String, (PathBuf, bool)> = BTreeMap::new();
    // Visibility of `struct Ctx { ... }` declarations the scanner
    // sees. When the user's ctx is `pub(crate)` (or no `pub`), the
    // generated `pub struct CtxRudzioBridge { pub inner: Ctx }`
    // would expose a less-private type than its only field, which
    // rustc rejects under `private_interfaces`. We mirror the user's
    // visibility on the bridge so the inequality goes away.
    let mut ctx_visibility: BTreeMap<String, Visibility> = BTreeMap::new();

    for file in pkg.src_files.iter().chain(pkg.tests_files.iter()) {
        let source =
            fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let Ok(tree) = syn::parse_file(&source) else {
            continue;
        };
        let mut scanner = Scanner {
            ctx_visibility: &mut ctx_visibility,
            current_file: file,
            impls: &mut impls,
            use_sites: &mut use_sites,
        };
        scanner.visit_file(&tree);
    }

    for key in &use_sites {
        match impls.get(key) {
            Some((impl_file, is_async)) => {
                let ctx_ident = last_segment(key);
                let bridge_ident = format!("{ctx_ident}RudzioBridge");
                let suite_ident = format!("{ctx_ident}RudzioSuite");
                let module_path = infer_module_path(impl_file, &pkg.root);
                let visibility = render_visibility(ctx_visibility.get(&ctx_ident));
                let plan = Plan {
                    bridge_ident,
                    ctx_ident,
                    ctx_key: key.clone(),
                    ctx_visibility: visibility,
                    impl_file: impl_file.clone(),
                    is_async: *is_async,
                    module_path,
                    suite_ident,
                };
                let _prev = resolver.plans.insert(key.clone(), plan);
            }
            None => {
                let _inserted = resolver.unresolved.insert(key.clone());
            }
        }
    }
    Ok(())
}

/// Render the bridge wrapper struct + its `Deref`/`DerefMut`/`Debug`
/// impls. Produced as a substring; concatenated with the suite block by
/// [`render_bridge_impls`].
fn render_bridge_block(ctx: &str, bridge: &str, vis: &str) -> String {
    format!(
        "\
/// Generated by rudzio-migrate: bridge wrapper for `{ctx}`. Takes the
/// `<'test_context, R>` generics rudzio's `#[rudzio::test]` macro
/// injects into ctx-param types, while the inner field is still your
/// original `{ctx}` (field access works via `Deref`/`DerefMut`).
{vis}struct {bridge}<'test_context, R>
where
    R: ::rudzio::Runtime<'test_context> + ::core::marker::Sync,
{{
    {vis}inner: {ctx},
    _marker: ::core::marker::PhantomData<&'test_context R>,
}}

impl<'test_context, R> ::core::ops::Deref for {bridge}<'test_context, R>
where
    R: ::rudzio::Runtime<'test_context> + ::core::marker::Sync,
{{
    type Target = {ctx};
    fn deref(&self) -> &{ctx} {{ &self.inner }}
}}

impl<'test_context, R> ::core::ops::DerefMut for {bridge}<'test_context, R>
where
    R: ::rudzio::Runtime<'test_context> + ::core::marker::Sync,
{{
    fn deref_mut(&mut self) -> &mut {ctx} {{ &mut self.inner }}
}}

impl<'test_context, R> ::core::fmt::Debug for {bridge}<'test_context, R>
where
    R: ::rudzio::Runtime<'test_context> + ::core::marker::Sync,
{{
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{
        f.debug_struct(\"{bridge}\").finish_non_exhaustive()
    }}
}}
"
    )
}

/// Render the suite struct + its `Suite`/`Test`/`Debug` impls. Produced
/// as a substring; concatenated with the bridge block by
/// [`render_bridge_impls`].
fn render_suite_block(
    bridge: &str,
    suite: &str,
    vis: &str,
    setup_call: &str,
    teardown_call: &str,
) -> String {
    format!(
        "
/// Generated by rudzio-migrate: bridge suite type so
/// `#[rudzio::suite([...])]` can reference a concrete Suite impl that
/// resolves to the `{bridge}` wrapper for each test. The generics
/// mirror what rudzio's `#[rudzio::suite(...)]` attribute expects: a
/// lifetime and a `Runtime`-bounded type parameter, both injected
/// invisibly at the callsite.
{vis}struct {suite}<'suite_context, R>
where
    R: for<'__r> ::rudzio::Runtime<'__r> + ::core::marker::Sync,
{{
    _marker: ::core::marker::PhantomData<&'suite_context R>,
}}

impl<'suite_context, R> ::core::fmt::Debug for {suite}<'suite_context, R>
where
    R: for<'__r> ::rudzio::Runtime<'__r> + ::core::marker::Sync,
{{
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{
        f.debug_struct(\"{suite}\").finish_non_exhaustive()
    }}
}}

impl<'suite_context, R> ::rudzio::Suite<'suite_context, R> for {suite}<'suite_context, R>
where
    R: for<'__r> ::rudzio::Runtime<'__r> + ::core::marker::Sync,
{{
    type ContextError = ::rudzio::BoxError;
    type SetupError = ::rudzio::BoxError;
    type TeardownError = ::rudzio::BoxError;

    type Test<'test_context>
        = {bridge}<'test_context, R>
    where
        Self: 'test_context;

    fn setup(
        _runtime: &'suite_context R,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> impl ::core::future::Future<Output = ::core::result::Result<Self, Self::SetupError>>
        + ::core::marker::Send
        + 'suite_context {{
        async move {{
            ::core::result::Result::Ok({suite} {{ _marker: ::core::marker::PhantomData }})
        }}
    }}

    fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> impl ::core::future::Future<
        Output = ::core::result::Result<{bridge}<'test_context, R>, Self::ContextError>,
    > + ::core::marker::Send
       + 'test_context {{
        async move {{
            ::core::result::Result::Ok({bridge} {{
                inner: {setup_call},
                _marker: ::core::marker::PhantomData,
            }})
        }}
    }}

    fn teardown(
        self,
    ) -> impl ::core::future::Future<Output = ::core::result::Result<(), Self::TeardownError>>
        + ::core::marker::Send
        + 'suite_context {{
        async move {{ ::core::result::Result::Ok(()) }}
    }}
}}

impl<'test_context, R> ::rudzio::Test<'test_context, R> for {bridge}<'test_context, R>
where
    R: ::rudzio::Runtime<'test_context> + ::core::marker::Sync,
{{
    type TeardownError = ::rudzio::BoxError;

    fn teardown(
        self,
    ) -> impl ::core::future::Future<Output = ::core::result::Result<(), Self::TeardownError>>
        + ::core::marker::Send
        + 'test_context {{
        async move {{
            {teardown_call}
            ::core::result::Result::Ok(())
        }}
    }}
}}
"
    )
}
