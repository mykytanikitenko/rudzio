use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::token::{Paren, Pub};
use syn::{AttrStyle, Attribute, Expr, Ident, Item, ItemFn, ItemMod, LitInt, Path, Token};

use crate::codegen::{TestAttrArgs, extract_ignore_reason, extract_test_attr_args};
use crate::parse::{MainArgs, RuntimeConfig};
use crate::transform::{
    CtxKind, apply_runtime_generics, classify_ctx_param, has_test_attr, is_async_fn, is_test_attr,
    strip_test_attrs,
};

/// Where a test fn's registration items (`run_test_fn` + token static)
/// are emitted. Direct `#[rudzio::test]` fns are processed by the suite
/// macro and registered at the suite's *outer* scope (siblings of the
/// `mod tests` block); `macro_rules!`-generated fns are registered by the
/// `#[rudzio::test]` attribute *inside* the module. The two sites differ
/// in how they name the test fn, the owner static, and the module path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistrationSite {
    /// Items emitted inside the suite module. `macro_rules!`-generated fns.
    Inner,
    /// Items emitted outside the suite module. Direct test fns.
    Outer,
}

/// Bundle of inputs for [`generate_per_test`]; gathered into a struct so
/// the helper does not exceed clippy's `too_many_arguments` threshold.
struct GeneratePerTestArgs<'args> {
    /// Outer attributes (`#[cfg(...)]` etc.) attached to the runtime-config
    /// entry; emitted on every item this helper produces so the entire
    /// per-entry expansion vanishes when the cfg doesn't hold.
    cfg_attrs: &'args [Attribute],
    /// Index of the parent runtime-config block within the suite macro.
    cfg_idx: usize,
    /// Identifier of the wrapping `mod tests` block.
    mod_name: &'args Ident,
    /// Identifier of the per-config `RuntimeGroupOwner` static instance.
    owner_static: &'args Ident,
    /// Concrete runtime type for this group.
    runtime_type: &'args Path,
    /// Whether the registration items land outside or inside the suite
    /// module (direct fns vs `macro_rules!`-generated fns).
    site: RegistrationSite,
    /// Source line of the test fn's identifier, for panic-hook attribution.
    /// Computed by the caller from the compiler span (the `#[rudzio::test]`
    /// attribute has the reliable span; re-parsed fns forwarded through
    /// `macro_rules!` may have re-synthesised spans).
    source_line: u32,
    /// Concrete suite type for this group.
    suite_base: &'args Path,
    /// The user's `#[rudzio::test]` fn item.
    test: &'args ItemFn,
}

/// Bundle of inputs for [`run_test_fn_quote`]; gathered into a struct so
/// the helper does not exceed clippy's `too_many_arguments` threshold.
struct RunTestFnArgs<'args> {
    /// Per-test attribute override for `setup_timeout_secs`, emitted as
    /// an `Option<u64>` const expression.
    attr_setup_timeout_secs: &'args TokenStream,
    /// Per-test attribute override for `teardown_timeout_secs`, emitted
    /// as an `Option<u64>` const expression.
    attr_teardown_timeout_secs: &'args TokenStream,
    /// Per-test attribute override for `timeout_secs`, emitted as an
    /// `Option<u64>` const expression.
    attr_test_timeout_secs: &'args TokenStream,
    /// Outer attributes (`#[cfg(...)]` etc.) attached to the runtime-config
    /// entry; emitted on the generated `unsafe fn` so the helper is
    /// elided on non-matching targets.
    cfg_attrs: &'args [Attribute],
    /// `let ctx` or `let mut ctx`, depending on the test's ctx kind.
    ctx_binding: &'args TokenStream,
    /// Identifier of the per-test `unsafe fn` HRTB run-test entry point.
    run_test_fn: &'args Ident,
    /// Concrete runtime type for this group.
    runtime_type: &'args Path,
    /// Concrete suite type for this group.
    suite_base: &'args Path,
    /// Either `plain_test_outcome` or `bench_test_outcome` token stream
    /// to be embedded in the per-test body.
    test_outcome_expr: &'args TokenStream,
}

/// Bundle of inputs for [`runtime_group_owner_impl`]; gathered into a
/// struct so the helper does not exceed clippy's `too_many_arguments`
/// threshold.
struct RuntimeGroupOwnerImplArgs<'args> {
    /// Outer attributes (`#[cfg(...)]` etc.) attached to the runtime-config
    /// entry; emitted on every item this helper produces so the entire
    /// per-entry expansion vanishes when the cfg doesn't hold.
    cfg_attrs: &'args [Attribute],
    /// Identifier of the wrapping `mod tests` block.
    mod_name: &'args Ident,
    /// Identifier of the per-config `RuntimeGroupOwner` static instance.
    owner_static: &'args Ident,
    /// Identifier of the per-config `RuntimeGroupOwner` ZST.
    owner_struct: &'args Ident,
    /// Path to the runtime constructor (e.g. `Multithread::new`).
    runtime_ctor: &'args Path,
    /// Concrete runtime type for this group.
    runtime_type: &'args Path,
    /// Concrete suite type for this group.
    suite_base: &'args Path,
}

/// Input parsed by [`expand_register_test`]: one `(runtime, suite)`
/// configuration plus a concrete test fn, forwarded by the hidden
/// `__rudzio_register_test!` macro.
struct RegisterTestInput {
    /// Index of this configuration within the suite's config list.
    cfg_idx: usize,
    /// Identifier of the wrapping `mod tests` block (drives generated idents).
    mod_name: Ident,
    /// Per-config `RuntimeGroupOwner` static ident (referenced as `super::`).
    owner: Ident,
    /// Concrete runtime type for this group.
    runtime_type: Path,
    /// Source line of the test fn identifier (passed explicitly; the fn has
    /// round-tripped through `macro_rules!` and may carry re-synthesised spans).
    source_line: u32,
    /// Concrete suite type for this group.
    suite_base: Path,
    /// The user's `#[rudzio::test]` fn, with the marker re-attached so the
    /// shared per-test codegen can read `#[ignore]` / `#[rudzio::test(..)]`.
    test: ItemFn,
}

impl Parse for RegisterTestInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mod_name = parse_named_ident(input, "mod_name")?;
        let source_line = parse_named_u32(input, "source_line")?;
        let cfg_idx = parse_named_usize(input, "cfg_idx")?;
        let runtime_type = parse_named_path(input, "runtime")?;
        let suite_base = parse_named_path(input, "suite")?;
        let owner = parse_named_ident(input, "owner")?;
        let test = parse_named_item_fn(input, "test")?;
        Ok(Self {
            cfg_idx,
            mod_name,
            owner,
            runtime_type,
            source_line,
            suite_base,
            test,
        })
    }
}

/// The two token streams `expand_suite` splices to arm a module for
/// `macro_rules!`-generated test registration.
struct MacroRegistrationSupport {
    /// `macro_rules! __rudzio_register_test! { ... }` placed inside the mod.
    inner_macro_rules: TokenStream,
    /// Per-config type aliases + the module-path const, placed at outer scope.
    outer_items: TokenStream,
}

/// Convert each per-test attribute timeout (`timeout_secs`,
/// `setup_timeout_secs`, `teardown_timeout_secs`) into an
/// `Option<u64>` const expression token stream. The runtime-side
/// resolver then computes
/// `override.map(Duration::from_secs).or(config.<phase>_timeout)`.
fn attr_timeout_overrides(attr_args: &TestAttrArgs) -> (TokenStream, TokenStream, TokenStream) {
    let to_quote = |secs: Option<u64>| -> TokenStream {
        secs.map_or_else(
            || quote! { ::core::option::Option::None },
            |seconds| quote! { ::core::option::Option::Some(#seconds) },
        )
    };
    (
        to_quote(attr_args.timeout_secs),
        to_quote(attr_args.setup_timeout_secs),
        to_quote(attr_args.teardown_timeout_secs),
    )
}

/// Token stream for a benchmarked test body dispatch: emits a
/// `match config.bench_mode { … }` so the same binary serves both
/// `cargo test` (Smoke / Skip) and `cargo test -- --bench` (Full).
fn bench_test_outcome(
    bench_expr: &Expr,
    dispatch_call: &TokenStream,
    runtime_type: &Path,
    bench_body_closure: &TokenStream,
) -> TokenStream {
    quote! {
        match config.bench_mode {
            ::rudzio::config::BenchMode::Full => {
                use ::rudzio::bench::Strategy as _;
                let __rudzio_strategy = #bench_expr;
                let __rudzio_progress_test_id = __rudzio_test_id;
                let bench_fut = __rudzio_strategy.run(
                    #bench_body_closure,
                    move |__rudzio_snapshot| {
                        ::rudzio::output::send_lifecycle(
                            ::rudzio::output::events::LifecycleEvent::BenchProgress {
                                test_id: __rudzio_progress_test_id,
                                snapshot: __rudzio_snapshot,
                            },
                        );
                    },
                );
                ::rudzio::suite::run_bench_with_timeout_and_cancel(
                    bench_fut,
                    test_timeout,
                    config.phase_hang_grace,
                    per_test_token.clone(),
                    |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(rt, dur),
                ).await
            }
            ::rudzio::config::BenchMode::Smoke
            | ::rudzio::config::BenchMode::Skip => {
                let test_fut = async { #dispatch_call };
                ::rudzio::suite::run_test_with_timeout_and_cancel(
                    test_fut,
                    test_timeout,
                    config.phase_hang_grace,
                    per_test_token.clone(),
                    |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(rt, dur),
                ).await
            }
            _ => {
                let test_fut = async { #dispatch_call };
                ::rudzio::suite::run_test_with_timeout_and_cancel(
                    test_fut,
                    test_timeout,
                    config.phase_hang_grace,
                    per_test_token.clone(),
                    |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(rt, dur),
                ).await
            }
        }
    }
}

/// Build the two dispatch-call token streams the per-test helper
/// embeds: a one-shot `dispatch_call` for the plain (non-bench)
/// outcome path, and a `bench_body_closure` for the strategy's
/// repeated-call invocation. Both route the body's return through
/// `IntoRudzioResult` and inline `&ctx` / `&mut ctx` into the call
/// site so no intermediate let-binding extends the borrow past the
/// `.await`.
fn dispatch_call_quotes(
    test_fn_path: &TokenStream,
    ctx_kind: CtxKind,
    is_async: bool,
) -> (TokenStream, TokenStream) {
    let dispatch_call_args = match ctx_kind {
        CtxKind::None => quote! {},
        CtxKind::Shared => quote! { &ctx },
        CtxKind::Mutable => quote! { &mut ctx },
    };
    let dispatch_call = if is_async {
        quote! {
            {
                use ::rudzio::IntoRudzioResult as _;
                #test_fn_path(#dispatch_call_args).await.into_rudzio_result()
            }
        }
    } else {
        quote! {
            {
                use ::rudzio::IntoRudzioResult as _;
                #test_fn_path(#dispatch_call_args).into_rudzio_result()
            }
        }
    };
    let bench_body_closure = if is_async {
        quote! {
            || async {
                use ::rudzio::IntoRudzioResult as _;
                #test_fn_path(#dispatch_call_args).await.into_rudzio_result()
            }
        }
    } else {
        quote! {
            || async {
                use ::rudzio::IntoRudzioResult as _;
                #test_fn_path(#dispatch_call_args).into_rudzio_result()
            }
        }
    };
    (dispatch_call, bench_body_closure)
}

/// Token-level entry point for the `#[rudzio::suite(...)]` proc-macro:
/// parses raw `args` / `input` token streams and dispatches to
/// [`expand_suite`].
///
/// Lives in this crate so the parsing + error-rendering logic stays
/// out of the `proc-macro = true` wrapper crate (whose `lib.rs` is
/// constrained by the Rust language to host the entry-point function
/// signatures and nothing else).
///
/// # Errors
///
/// - `args` does not parse as a [`MainArgs`].
/// - `input` does not parse as an [`ItemMod`].
/// - [`expand_suite`] returns an error.
#[inline]
pub fn expand_entry(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let parsed_args: MainArgs = syn::parse2(args)?;
    let input_mod: ItemMod = syn::parse2(input)?;
    expand_suite(&parsed_args, input_mod)
}

/// Expand a `#[rudzio::suite([...])] mod ... { ... }` invocation into the
/// fully wired per-runtime helper items, lifecycle/test statics, and
/// rewritten test module.
///
/// # Errors
///
/// Returns `Err(syn::Error)` when:
/// - the input module has no body (the user wrote `mod foo;` instead
///   of `mod foo { ... }`),
/// - the module body contains no `#[rudzio::test]`-annotated functions,
/// - or any per-test attribute body fails to parse (propagated from
///   [`crate::codegen::extract_test_attr_args`]).
#[inline]
pub fn expand_suite(args: &MainArgs, input_mod: ItemMod) -> syn::Result<TokenStream> {
    let items = match &input_mod.content {
        Some((_, items)) => items.clone(),
        None => {
            return Err(syn::Error::new_spanned(
                input_mod,
                "expected module body, found empty module",
            ));
        }
    };

    // Partition the input mod's attributes by AttrStyle so the regenerated
    // mod block places each at a syntactically valid position. Pre-fix,
    // every attribute -- including inner attrs styled as `#![...]` --
    // got splatted before the `mod` keyword, and rustc rejected the
    // inner ones with `error: an inner attribute is not permitted in
    // this context`. Outer attrs (`#[cfg(...)]`, etc.) belong on the
    // mod item itself; inner attrs (`#![allow(...)]`, `//!` doc
    // comments, etc.) belong INSIDE the regenerated mod body so they
    // bind to the mod contents the user wrote them against.
    let (mod_outer_attrs, mod_inner_attrs): (Vec<&Attribute>, Vec<&Attribute>) = input_mod
        .attrs
        .iter()
        .partition(|attr| matches!(attr.style, AttrStyle::Outer));
    let mod_vis = &input_mod.vis;
    let mod_name = &input_mod.ident;

    let processed_items: Vec<_> = items
        .iter()
        .map(|item| {
            if let Item::Fn(func) = item
                && has_test_attr(func)
            {
                let mut modified = func.clone();
                modified.vis = syn::Visibility::Restricted(syn::VisRestricted {
                    pub_token: Pub(Span::call_site()),
                    paren_token: Paren(Span::call_site()),
                    in_token: None,
                    path: Box::new(Path::from(syn::PathSegment::from(Ident::new(
                        "super",
                        Span::call_site(),
                    )))),
                });
                modified.attrs.retain(|attr| !is_test_attr(attr));
                modified = apply_runtime_generics(modified);
                return Item::Fn(modified);
            }
            item.clone()
        })
        .collect();

    let test_functions: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let Item::Fn(func) = item
                && has_test_attr(func)
            {
                Some(func.clone())
            } else {
                None
            }
        })
        .collect();

    // A `macro_rules!` invocation in item position (`make_test!(a);`) is an
    // opaque `Item::Macro` pre-expansion: it may expand into `#[rudzio::test]`
    // fns the suite macro cannot see. Its presence keeps the module valid even
    // when `test_functions` is empty — those fns are registered later by the
    // `#[rudzio::test]` attribute.
    let has_macro_items = items.iter().any(|item| matches!(item, Item::Macro(_)));
    // This "no test functions found" check runs BEFORE cfg evaluation: a suite
    // whose every direct test is `#[cfg]`-gated off (down to zero active
    // tests) is still valid, since the runtime short-circuits on an empty
    // `active` list (see `owner_runtime_init_quote`). It is therefore not a
    // macro-time error.
    if test_functions.is_empty() && !has_macro_items {
        return Err(syn::Error::new_spanned(
            input_mod,
            "no test functions found in module - add functions with #[rudzio::test] attribute",
        ));
    }

    let mut helper_items: Vec<TokenStream> = vec![];
    let mut token_statics: Vec<TokenStream> = vec![];

    for (cfg_idx, cfg) in args.configs.iter().enumerate() {
        generate_per_config(
            mod_name,
            cfg_idx,
            cfg,
            &test_functions,
            &mut helper_items,
            &mut token_statics,
        )?;
    }

    // When the module body contains `macro_rules!` invocations, arm the
    // module with the hidden forwarding macro + outer module-path const so
    // the `#[rudzio::test]` attribute (running post-expansion on the
    // generated fns) can register them against this suite's configs.
    let (inner_macro_rules, outer_items) = if has_macro_items {
        let support = generate_macro_registration_support(mod_name, args)?;
        (support.inner_macro_rules, support.outer_items)
    } else {
        (TokenStream::new(), TokenStream::new())
    };

    let expanded = quote! {
        #(#mod_outer_attrs)*
        #mod_vis mod #mod_name {
            #(#mod_inner_attrs)*
            #inner_macro_rules
            #(#processed_items)*
        }

        #outer_items

        #(#helper_items)*

        #(#token_statics)*
    };

    Ok(expanded)
}

/// Emit the per-config (i.e. per `(runtime, suite, test)` triple) helper
/// items and lifecycle/test statics for one `RuntimeConfig` of a
/// `#[rudzio::main]` invocation. Mutates `helper_items` and
/// `token_statics` in place — both are flushed into the final expansion
/// by `expand_suite`.
fn generate_per_config(
    mod_name: &Ident,
    cfg_idx: usize,
    cfg: &RuntimeConfig,
    tests: &[ItemFn],
    helper_items: &mut Vec<TokenStream>,
    token_statics: &mut Vec<TokenStream>,
) -> syn::Result<()> {
    let cfg_attrs = cfg.attrs.as_slice();
    let runtime_ctor = &cfg.runtime;
    let runtime_type = cfg.runtime_type()?;
    let suite_base = &cfg.suite;

    // Stable id derived from `TypeId::of::<R>()` and `TypeId::of::<S>()` —
    // emitted as `RuntimeGroupKey::of::<R, S>()` at both the owner's
    // `group_key()` site and every per-test token static. Two suite blocks
    // declaring the same concrete `(R, S)` get the same key (and share one
    // OS thread / runtime / suite at runtime) regardless of path-string
    // spelling; two blocks declaring different types with byte-identical
    // attribute spelling get distinct keys, preventing the cross-type
    // owner-cast UB that string-based keys allowed.

    let mod_camel = to_upper_camel(&mod_name.to_string());
    let owner_struct = format_ident!("__RudzioOwner{}{}", mod_camel, cfg_idx);
    let owner_static = format_ident!(
        "__RUDZIO_OWNER_{}_{}",
        mod_name.to_string().to_ascii_uppercase(),
        cfg_idx,
    );

    helper_items.push(runtime_group_owner_impl(&RuntimeGroupOwnerImplArgs {
        cfg_attrs,
        mod_name,
        owner_static: &owner_static,
        owner_struct: &owner_struct,
        runtime_ctor,
        runtime_type: &runtime_type,
        suite_base,
    }));

    for test in tests {
        let source_line = source_line_of(test)?;
        let (helper, token_static) = generate_per_test(&GeneratePerTestArgs {
            cfg_attrs,
            cfg_idx,
            mod_name,
            owner_static: &owner_static,
            runtime_type: &runtime_type,
            suite_base,
            site: RegistrationSite::Outer,
            source_line,
            test,
        })?;
        helper_items.push(helper);
        token_statics.push(token_static);
    }

    Ok(())
}

/// Collect a test fn's own `#[cfg(...)]` attributes — the only attribute
/// form that can conditionally *remove* the fn during cfg-stripping. The
/// generated `run_test_fn` helper and token static must carry the same
/// predicates so a cfg-stripped test does not leave dangling registration
/// items naming it (E0425). `#[cfg_attr(...)]` is deliberately NOT
/// propagated: it only adds (never removes) attributes, so it cannot strip
/// the fn, and re-emitting it verbatim on the token `static` would hard-error
/// when it wraps `ignore` (rustc rejects `#[ignore]` on a `static`). That case
/// is handled separately through the token's `ignored` flag.
fn test_cfg_attrs(test: &ItemFn) -> Vec<Attribute> {
    test.attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg"))
        .cloned()
        .collect()
}

/// Emit the `(helper_item, token_static)` pair for a single
/// `#[rudzio::test]` fn. Pulled out of [`generate_per_config`]'s loop body
/// to keep that fn readable and within clippy's per-fn line budget.
fn generate_per_test(args: &GeneratePerTestArgs<'_>) -> syn::Result<(TokenStream, TokenStream)> {
    let &GeneratePerTestArgs {
        cfg_attrs,
        cfg_idx,
        mod_name,
        owner_static,
        runtime_type,
        suite_base,
        site,
        source_line,
        test,
    } = args;

    let test_name = &test.sig.ident;
    let test_name_str = test_name.to_string();

    // The test fn's own `#[cfg(...)]` predicates gate its existence; the
    // generated helper fn and token static must carry the same predicates so
    // cfg-stripping removes them together with the fn (E0425). The
    // runtime-config `cfg_attrs` are prepended so both gate sources apply.
    let combined_cfg_attrs: Vec<Attribute> = cfg_attrs
        .iter()
        .cloned()
        .chain(test_cfg_attrs(test))
        .collect();

    let (ignored, ignore_reason) = extract_ignore_reason(test);
    let attr_args = extract_test_attr_args(test)?;
    let (attr_test_timeout_secs, attr_setup_timeout_secs, attr_teardown_timeout_secs) =
        attr_timeout_overrides(&attr_args);
    let benchmark = attr_args.benchmark;
    let has_benchmark = benchmark.is_some();
    let is_async = is_async_fn(test);
    let ctx_kind = classify_ctx_param(test);

    // `benchmark = ...` calls the body many times concurrently and/or
    // requires an `Fn`-style closure that can produce the future
    // repeatedly. A `&mut ctx` signature would force exclusive access
    // on every call, which the borrow checker rejects. Reject at macro
    // time with a clear message instead of a cryptic borrow-checker
    // diagnostic on generated code.
    if has_benchmark && matches!(ctx_kind, CtxKind::Mutable) {
        return Err(syn::Error::new_spanned(
            test,
            "`#[rudzio::test(benchmark = ...)]` requires a `&Ctx` or no-ctx \
             signature; benchmarks call the body repeatedly and `&mut Ctx` \
             would force exclusive access across iterations",
        ));
    }

    let (token_static_ident, run_test_fn) = per_test_idents(mod_name, test_name, cfg_idx);
    // `ctx` is bound in every branch because the runner always runs
    // per-test teardown (`ctx.teardown()`) — that's the whole point
    // of the `Test` trait, regardless of whether the test body
    // takes a context parameter.
    let ctx_binding = match ctx_kind {
        CtxKind::Mutable => quote! { let mut ctx },
        CtxKind::Shared | CtxKind::None => quote! { let ctx },
    };

    let (test_fn_path, owner_ref, module_path_expr) =
        registration_paths(site, mod_name, test_name, owner_static);

    let (dispatch_call, bench_body_closure) =
        dispatch_call_quotes(&test_fn_path, ctx_kind, is_async);

    let test_outcome_expr = benchmark.as_ref().map_or_else(
        || plain_test_outcome(&dispatch_call, runtime_type),
        |bench_expr| {
            bench_test_outcome(
                bench_expr,
                &dispatch_call,
                runtime_type,
                &bench_body_closure,
            )
        },
    );

    let helper_item = run_test_fn_quote(&RunTestFnArgs {
        cfg_attrs: combined_cfg_attrs.as_slice(),
        run_test_fn: &run_test_fn,
        runtime_type,
        suite_base,
        attr_test_timeout_secs: &attr_test_timeout_secs,
        attr_setup_timeout_secs: &attr_setup_timeout_secs,
        attr_teardown_timeout_secs: &attr_teardown_timeout_secs,
        ctx_binding: &ctx_binding,
        test_outcome_expr: &test_outcome_expr,
    });

    let token_static = quote! {
        #(#combined_cfg_attrs)*
        #[::rudzio::linkme::distributed_slice(::rudzio::token::TEST_TOKENS)]
        #[linkme(crate = ::rudzio::linkme)]
        #[doc(hidden)]
        static #token_static_ident: ::rudzio::token::Token = ::rudzio::token::Token::new(
            ::rudzio::token::Source::new(
                ::std::file!(),
                #source_line,
                #module_path_expr,
                #test_name_str,
            ),
            ::rudzio::token::Attrs::new(
                #has_benchmark,
                #ignore_reason,
                #ignored,
            ),
            ::rudzio::token::Dispatch::new(
                #run_test_fn,
                ::rudzio::suite::RuntimeGroupKey::of::<
                    #runtime_type,
                    #suite_base::<'static, #runtime_type>,
                >(),
                &#owner_ref,
            ),
        );
    };

    Ok((helper_item, token_static))
}

/// Token stream for the dispatch loop: builds the safety-cast
/// `runtime_ptr` / `suite_ptr`, the per-token `dispatch` closure, the
/// initial seeding of `in_flight` up to `concurrency_limit`, and the
/// `while let` drain that updates `summary` and refills `in_flight` as
/// futures resolve. Tokens left in `queued` after the drain are
/// reported as cancelled.
fn owner_dispatch_loop_quote(suite_base: &Path, runtime_type: &Path) -> TokenStream {
    quote! {
        // Pointers we hand to each per-test fn pointer. The
        // pointed-to types match the token's group_key —
        // guaranteed by the macro that emits both sides.
        let runtime_ptr: *const () = (&rt as *const #runtime_type).cast::<()>();
        let suite_ptr: *const () =
            (&suite as *const #suite_base::<'_, #runtime_type>).cast::<()>();

        let mut in_flight = FuturesUnordered::new();
        let mut queued = active.into_iter();

        let dispatch = |tok: &'static ::rudzio::token::Token| {
            // SAFETY: `tok.run_test` was emitted by the same
            // suite macro that emitted this owner; its
            // `runtime_group_key` matches `self.group_key()`,
            // so the pointed-to types are exactly those the
            // fn ptr expects.
            #[allow(unsafe_code)]
            unsafe {
                (tok.run_test)(
                    runtime_ptr,
                    suite_ptr,
                    ::std::marker::PhantomData,
                    tok,
                    req.config.test_timeout,
                    req.root_token.clone(),
                    reporter,
                )
            }
        };

        if !req.root_token.is_cancelled() {
            for _ in 0..req.config.concurrency_limit {
                let ::std::option::Option::Some(tok) = queued.next() else { break };
                let fut: ::std::pin::Pin<::std::boxed::Box<
                    dyn ::std::future::Future<
                        Output = (
                            &'static ::rudzio::token::Token,
                            ::rudzio::suite::TestOutcome,
                        ),
                    > + '_,
                >> = ::std::boxed::Box::pin(async move {
                    let outcome = dispatch(tok).await;
                    (tok, outcome)
                });
                in_flight.push(fut);
            }
        }

        while let ::std::option::Option::Some((tok, outcome)) = in_flight.next().await {
            match &outcome {
                ::rudzio::suite::TestOutcome::Passed { .. } => summary.passed += 1,
                ::rudzio::suite::TestOutcome::Failed { .. }
                | ::rudzio::suite::TestOutcome::SetupFailed { .. } => {
                    summary.failed += 1;
                }
                ::rudzio::suite::TestOutcome::Panicked { .. } => summary.panicked += 1,
                ::rudzio::suite::TestOutcome::TimedOut => summary.timed_out += 1,
                ::rudzio::suite::TestOutcome::Hung { .. } => summary.hung += 1,
                ::rudzio::suite::TestOutcome::Cancelled => summary.cancelled += 1,
                ::rudzio::suite::TestOutcome::Benched { report, .. } => {
                    if report.is_success() {
                        summary.passed += 1;
                    } else {
                        summary.failed += 1;
                    }
                }
                _ => summary.failed += 1,
            }
            reporter.report_outcome(tok, runtime_name, outcome);
            if !req.root_token.is_cancelled()
                && let ::std::option::Option::Some(next) = queued.next()
            {
                let fut: ::std::pin::Pin<::std::boxed::Box<
                    dyn ::std::future::Future<
                        Output = (
                            &'static ::rudzio::token::Token,
                            ::rudzio::suite::TestOutcome,
                        ),
                    > + '_,
                >> = ::std::boxed::Box::pin(async move {
                    let outcome = dispatch(next).await;
                    (next, outcome)
                });
                in_flight.push(fut);
            }
        }

        for skipped in queued {
            reporter.report_cancelled(skipped, runtime_name);
            summary.cancelled += 1;
        }

        // Drop in_flight before consuming suite; the now-empty
        // FuturesUnordered would otherwise still be considered
        // a live borrow.
        ::std::mem::drop(in_flight);
    }
}

/// Token stream for the run-group preamble that runs *before*
/// `Runtime::block_on`: creates the runtime via the user's ctor,
/// derives the runtime + suite labels, classifies tokens into
/// `ignored` vs `active`, and short-circuits the entire group when
/// no token survived the classification.
fn owner_runtime_init_quote(
    runtime_ctor: &Path,
    runtime_type: &Path,
    mod_name: &Ident,
) -> TokenStream {
    quote! {
        // Step 1: create the runtime as a local value. Lives until
        // the end of `run_group`; nothing leaked to `'static`.
        // The constructor takes `&Config` so runtimes can adapt
        // (e.g. size their worker pool to `config.threads`).
        let rt: #runtime_type = match #runtime_ctor(req.config) {
            Ok(r) => r,
            Err(e) => {
                reporter.report_warning(&::std::format!(
                    "FATAL: failed to create runtime: {}", e,
                ));
                let mut summary = ::rudzio::suite::Summary::zero();
                summary.total = req.tokens.len();
                summary.panicked = req.tokens.len();
                return summary;
            }
        };

        // `Runtime::name` is the single source of truth for the
        // runtime's display label throughout this run.
        let runtime_name: &'static str =
            ::rudzio::runtime::Runtime::name(&rt);

        // Stable label for this suite — `module_path!()` at the
        // suite-macro expansion site, joined with the wrapped
        // module's identifier. Used by reporter lifecycle events
        // so the user can see which suite is in setup/teardown.
        let suite_label: &'static str = ::core::concat!(
            ::core::module_path!(),
            "::",
            ::core::stringify!(#mod_name),
        );

        // Step 2: classify ignored vs active. Bench-annotated tests
        // are reported as ignored (with a short reason) when
        // `--no-bench` is set, so the final summary still accounts
        // for every declared test.
        let mut summary = ::rudzio::suite::Summary::zero();
        summary.total = req.tokens.len();
        let mut active: ::std::vec::Vec<&'static ::rudzio::token::Token> =
            ::std::vec::Vec::with_capacity(req.tokens.len());
        for tok in req.tokens {
            let ignore_skip = match req.config.run_ignored {
                ::rudzio::config::RunIgnoredMode::Normal => tok.ignored,
                ::rudzio::config::RunIgnoredMode::Only
                | ::rudzio::config::RunIgnoredMode::Include => false,
                _ => tok.ignored,
            };
            let bench_skip = matches!(
                req.config.bench_mode,
                ::rudzio::config::BenchMode::Skip,
            ) && tok.has_benchmark;
            if ignore_skip || bench_skip {
                reporter.report_ignored(*tok, runtime_name);
                summary.ignored += 1;
            } else {
                active.push(*tok);
            }
        }

        // Nothing to run — every token was filtered to ignored or
        // bench-skipped. Skip the runtime's block_on entirely so we
        // don't pay setup/teardown cost for a suite with no active
        // tests. Tokens filtered out by the runner's positional /
        // --skip filters never reach here at all (the runner drops
        // their group from dispatch); this guard handles the
        // remaining case where every reaching token was classified
        // out above.
        if active.is_empty() {
            return summary;
        }
    }
}

/// Token stream that, given that `__rudzio_setup_finished_msg` has
/// already been invoked, drains every active token by reporting the
/// matching `TestOutcome` and incrementing the corresponding
/// `summary` counter, then returns the summary. Used as the tail of
/// every suite-setup failure arm.
fn owner_suite_setup_drain_quote(outcome: &TokenStream, counter_field: &Ident) -> TokenStream {
    quote! {
        for tok in active.iter() {
            reporter.report_outcome(
                *tok,
                runtime_name,
                #outcome,
            );
            summary.#counter_field += 1;
        }
        return summary;
    }
}

/// Token stream for the suite-setup phase inside `block_on`: drives
/// `Suite::setup` through the canonical phase helper, classifies
/// every `PhaseOutcome` variant, and on any non-`Completed-Ok`
/// outcome reports each active token as cancelled (or hung) and
/// returns the summary early — `let suite = match ...` succeeds only
/// on the happy path.
fn owner_suite_setup_quote(suite_base: &Path, runtime_type: &Path) -> TokenStream {
    let cancel_field = Ident::new("cancelled", Span::call_site());
    let hung_field = Ident::new("hung", Span::call_site());
    let drain_cancelled = owner_suite_setup_drain_quote(
        &quote! { ::rudzio::suite::TestOutcome::Cancelled },
        &cancel_field,
    );
    let drain_hung = owner_suite_setup_drain_quote(
        &quote! {
            ::rudzio::suite::TestOutcome::Hung {
                elapsed: ::std::time::Duration::ZERO,
            }
        },
        &hung_field,
    );
    quote! {
        reporter.report_suite_setup_started(runtime_name, suite_label);
        let __rudzio_setup_start = ::std::time::Instant::now();
        // Per-phase token: child of root, so root cancellation
        // (run-timeout, SIGINT) still propagates here. The
        // wrapper cancels this token if `--suite-setup-timeout`
        // fires, dropping the in-flight setup and signalling
        // any cooperative tasks the user spawned through it.
        let __rudzio_suite_setup_phase_token = req.root_token.child_token();
        let __rudzio_setup_phase = ::rudzio::suite::run_phase_with_timeout_and_cancel(
            <
                #suite_base::<'_, #runtime_type> as ::rudzio::context::Suite<'_, #runtime_type>
            >::setup(&rt, __rudzio_suite_setup_phase_token.clone(), req.config),
            req.config.suite_setup_timeout,
            req.config.phase_hang_grace,
            __rudzio_suite_setup_phase_token,
            |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(&rt, dur),
        ).await;
        let __rudzio_setup_elapsed = __rudzio_setup_start.elapsed();
        let suite = match __rudzio_setup_phase {
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Ok(s)) => {
                reporter.report_suite_setup_finished(
                    runtime_name, suite_label, __rudzio_setup_elapsed,
                    ::core::option::Option::None,
                );
                s
            }
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Err(e)) => {
                // Alternate Display (`{:#}`) prints the anyhow error chain
                // rather than just the top-level context, so users see
                // the actual cause of setup failures (e.g. the cargo
                // stderr from a fixture's pre-warm step) instead of a
                // bare `.context()` annotation.
                let __rudzio_err_msg = ::std::format!("{:#}", e);
                reporter.report_suite_setup_finished(
                    runtime_name, suite_label, __rudzio_setup_elapsed,
                    ::core::option::Option::Some(&__rudzio_err_msg),
                );
                #drain_cancelled
            }
            ::rudzio::suite::PhaseOutcome::Panicked(msg) => {
                let __rudzio_panic_msg = ::std::format!("panic: {}", msg);
                reporter.report_suite_setup_finished(
                    runtime_name, suite_label, __rudzio_setup_elapsed,
                    ::core::option::Option::Some(&__rudzio_panic_msg),
                );
                #drain_cancelled
            }
            ::rudzio::suite::PhaseOutcome::TimedOut => {
                let __rudzio_timeout_msg = ::std::string::String::from("setup timed out");
                reporter.report_suite_setup_finished(
                    runtime_name, suite_label, __rudzio_setup_elapsed,
                    ::core::option::Option::Some(&__rudzio_timeout_msg),
                );
                #drain_cancelled
            }
            ::rudzio::suite::PhaseOutcome::Hung => {
                let __rudzio_hung_msg = ::std::string::String::from("setup hung; abort signal sent");
                reporter.report_suite_setup_finished(
                    runtime_name, suite_label, __rudzio_setup_elapsed,
                    ::core::option::Option::Some(&__rudzio_hung_msg),
                );
                #drain_hung
            }
            ::rudzio::suite::PhaseOutcome::Cancelled => {
                // Root token cancelled (run-timeout / SIGINT)
                // — every active test reports cancelled and
                // we skip teardown (setup never finished).
                reporter.report_suite_setup_finished(
                    runtime_name, suite_label, __rudzio_setup_elapsed,
                    ::core::option::Option::Some("setup cancelled"),
                );
                #drain_cancelled
            }
            _ => {
                // Future PhaseOutcome variants treated as cancelled
                // (conservative default — every active test reports
                // cancelled and we skip teardown).
                reporter.report_suite_setup_finished(
                    runtime_name, suite_label, __rudzio_setup_elapsed,
                    ::core::option::Option::Some("setup unknown outcome"),
                );
                #drain_cancelled
            }
        };
    }
}

/// Token stream for the suite-teardown phase inside `block_on`:
/// drives `Suite::teardown` through the canonical phase helper with
/// a fresh unparented token, classifies the outcome into a
/// `TeardownResult`, and emits `report_suite_teardown_finished`.
fn owner_suite_teardown_quote(runtime_type: &Path) -> TokenStream {
    quote! {
        reporter.report_suite_teardown_started(runtime_name, suite_label);
        let __rudzio_teardown_start = ::std::time::Instant::now();
        // Fresh, unparented phase token. Cleanup must run
        // to completion regardless of run-timeout / SIGINT
        // — same guarantee as before the wrapper existed.
        // Only the per-suite-teardown timeout can
        // short-circuit it.
        let __rudzio_suite_teardown_phase_token =
            ::rudzio::tokio_util::sync::CancellationToken::new();
        let __rudzio_teardown_phase = ::rudzio::suite::run_phase_with_timeout_and_cancel(
            suite.teardown(__rudzio_suite_teardown_phase_token.clone()),
            req.config.suite_teardown_timeout,
            req.config.phase_hang_grace,
            __rudzio_suite_teardown_phase_token,
            |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(&rt, dur),
        ).await;
        let __rudzio_teardown_elapsed = __rudzio_teardown_start.elapsed();
        let __rudzio_teardown_result = match __rudzio_teardown_phase {
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Ok(())) => {
                ::rudzio::suite::TeardownResult::Ok
            }
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Err(e)) => {
                summary.teardown_failures += 1;
                // Alternate Display preserves the full error chain so
                // `.context()` annotations from the teardown body surface
                // alongside the root cause.
                ::rudzio::suite::TeardownResult::Err(::std::format!("{:#}", e))
            }
            ::rudzio::suite::PhaseOutcome::Panicked(msg) => {
                summary.teardown_failures += 1;
                ::rudzio::suite::TeardownResult::Panicked(msg)
            }
            ::rudzio::suite::PhaseOutcome::TimedOut => {
                summary.teardown_failures += 1;
                ::rudzio::suite::TeardownResult::TimedOut
            }
            ::rudzio::suite::PhaseOutcome::Hung => {
                summary.teardown_failures += 1;
                ::rudzio::suite::TeardownResult::Hung
            }
            ::rudzio::suite::PhaseOutcome::Cancelled => {
                summary.teardown_failures += 1;
                ::rudzio::suite::TeardownResult::Err(
                    ::std::string::String::from("teardown cancelled"),
                )
            }
            _ => {
                summary.teardown_failures += 1;
                ::rudzio::suite::TeardownResult::Err(
                    ::std::string::String::from("teardown unknown outcome"),
                )
            }
        };
        reporter.report_suite_teardown_finished(
            runtime_name,
            suite_label,
            __rudzio_teardown_elapsed,
            __rudzio_teardown_result,
        );
    }
}

/// Token stream for a non-benchmarked test body dispatch: a single
/// `run_test_with_timeout_and_cancel` call wrapped in a block.
fn plain_test_outcome(dispatch_call: &TokenStream, runtime_type: &Path) -> TokenStream {
    quote! {
        {
            let test_fut = async { #dispatch_call };
            ::rudzio::suite::run_test_with_timeout_and_cancel(
                test_fut,
                test_timeout,
                config.phase_hang_grace,
                per_test_token.clone(),
                |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(rt, dur),
            ).await
        }
    }
}

/// Emit the per-test `unsafe fn` HRTB run-test entry point. Pushed into
/// `helper_items` once per test; each fn ptr is invoked by the matching
/// `RuntimeGroupOwner::run_group` via `(tok.run_test)(...)` after the
/// macro guarantees the pointed-to runtime + suite types match.
fn run_test_fn_quote(args: &RunTestFnArgs<'_>) -> TokenStream {
    let &RunTestFnArgs {
        attr_setup_timeout_secs,
        attr_teardown_timeout_secs,
        attr_test_timeout_secs,
        cfg_attrs,
        ctx_binding,
        run_test_fn,
        runtime_type,
        suite_base,
        test_outcome_expr,
    } = args;
    let timeouts = run_test_timeouts_quote(
        attr_test_timeout_secs,
        attr_setup_timeout_secs,
        attr_teardown_timeout_secs,
    );
    let lifecycle_announce = run_test_lifecycle_announce_quote(runtime_type);
    let setup_phase = run_test_setup_phase_quote(ctx_binding, runtime_type);
    let teardown_phase = run_test_teardown_phase_quote(runtime_type);
    let lifecycle_complete = run_test_lifecycle_complete_quote();
    quote! {
        #(#cfg_attrs)*
        #[doc(hidden)]
        unsafe fn #run_test_fn<'s>(
            runtime_ptr: *const (),
            suite_ptr: *const (),
            _phantom: ::std::marker::PhantomData<&'s ()>,
            token: &'static ::rudzio::token::Token,
            test_timeout: ::std::option::Option<::std::time::Duration>,
            root_token: ::rudzio::tokio_util::sync::CancellationToken,
            reporter: &'s dyn ::rudzio::suite::Reporter,
        ) -> ::std::pin::Pin<::std::boxed::Box<
            dyn ::std::future::Future<Output = ::rudzio::suite::TestOutcome> + 's
        >> {
            use ::rudzio::context::Suite as _;
            use ::rudzio::context::Test as _;
            use ::rudzio::runtime::Runtime as _;
            use ::rudzio::futures_util::FutureExt as _;

            ::std::boxed::Box::pin(async move {
                // SAFETY: caller (the runtime group owner) hands us
                // pointers whose `runtime_group_key` matches this fn's;
                // the macro emitted both sides, so the concrete types
                // are `#runtime_type` and `#suite_base::<'s, …>`.
                #[allow(unsafe_code)]
                let rt: &'s #runtime_type =
                    unsafe { &*(runtime_ptr as *const #runtime_type) };
                #[allow(unsafe_code)]
                let suite: &'s #suite_base::<'s, #runtime_type> = unsafe {
                    &*(suite_ptr as *const #suite_base::<'s, #runtime_type>)
                };

                let start = ::std::time::Instant::now();
                let per_test_token = root_token.child_token();
                let config: &'s ::rudzio::config::Config =
                    ::rudzio::runtime::Runtime::config(rt);

                #timeouts
                #lifecycle_announce

                // Acquire one permit from the process-wide
                // `--threads-parallel-hardlimit` gate. The guard is
                // held across setup + body + teardown and dropped
                // just before the `TestCompleted` lifecycle emit, so
                // any parking notice the primitive writes lands in
                // this test's stdout-capture block and gets
                // attributed to the right TestId. `.await` yields
                // cooperatively when the gate is full so permit-
                // holders awaiting timers / IO / spawned subtasks
                // remain pollable.
                let __rudzio_hardlimit_guard =
                    config.acquire_hardlimit_permit().await;

                let outcome = 'run: {
                    #setup_phase

                    let test_outcome = #test_outcome_expr;

                    let outcome = ::rudzio::suite::fill_elapsed(
                        test_outcome,
                        start.elapsed(),
                    );

                    #teardown_phase

                    outcome
                };

                #lifecycle_complete

                outcome
            })
        }
    }
}

/// Token stream for the lifecycle "test started" announcement
/// preamble: allocates a `TestId`, parks it on the panic hook
/// thread-local, and emits `LifecycleEvent::TestStarted`.
fn run_test_lifecycle_announce_quote(runtime_type: &Path) -> TokenStream {
    quote! {
        // Allocate a per-dispatch TestId, register it in the
        // panic hook's thread-local, and announce the test to
        // the drawer. Paired with the `TestCompleted` emit
        // below so drawer state stays consistent even for
        // tests that fail during context setup.
        let __rudzio_test_id =
            ::rudzio::output::events::TestId::next();
        ::rudzio::output::panic_hook::set_current_test(
            ::std::option::Option::Some(__rudzio_test_id),
        );
        ::rudzio::output::send_lifecycle(
            ::rudzio::output::events::LifecycleEvent::TestStarted {
                test_id: __rudzio_test_id,
                module_path: token.module_path,
                test_name: token.name,
                runtime_name:
                    <#runtime_type as ::rudzio::runtime::Runtime<'_>>::name(rt),
                thread: ::std::thread::current().id(),
                at: start,
            },
        );
    }
}

/// Token stream for the lifecycle "test completed" tail: drops the
/// hardlimit permit guard, flushes producer-side stdio, emits
/// `LifecycleEvent::TestCompleted`, and clears the panic-hook test id.
fn run_test_lifecycle_complete_quote() -> TokenStream {
    quote! {
        // Release the parallel-hardlimit permit before the
        // TestCompleted emit so another parked test can wake
        // and start reporting without waiting on this test's
        // drawer bookkeeping.
        ::std::mem::drop(__rudzio_hardlimit_guard);

        // Flush producer-side stdio so all captured bytes are
        // in the drawer's pipe before announcing completion.
        {
            use ::std::io::Write as _;
            let _unused_stdout = ::std::io::stdout().flush();
            let _unused_stderr = ::std::io::stderr().flush();
        }
        ::rudzio::output::send_lifecycle(
            ::rudzio::output::events::LifecycleEvent::TestCompleted {
                test_id: __rudzio_test_id,
                outcome: ::std::clone::Clone::clone(&outcome),
            },
        );
        ::rudzio::output::panic_hook::set_current_test(
            ::std::option::Option::None,
        );
    }
}

/// Token stream for the per-test setup phase: invokes
/// `Suite::context` through the canonical phase helper and matches
/// every `PhaseOutcome` variant, breaking the surrounding `'run:`
/// block with the appropriate `TestOutcome` for failure paths.
fn run_test_setup_phase_quote(ctx_binding: &TokenStream, runtime_type: &Path) -> TokenStream {
    quote! {
        // Per-test setup wrapped in the canonical phase
        // helper: handles cancellation propagation, panic
        // catching, and the per-test-setup timeout.
        let __rudzio_ctx_phase =
            ::rudzio::suite::run_phase_with_timeout_and_cancel(
                suite.context(per_test_token.clone(), config),
                __rudzio_setup_timeout,
                config.phase_hang_grace,
                per_test_token.clone(),
                |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(rt, dur),
            ).await;
        #ctx_binding = match __rudzio_ctx_phase {
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Ok(c)) => c,
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Err(e)) => {
                break 'run ::rudzio::suite::TestOutcome::SetupFailed {
                    elapsed: start.elapsed(),
                    message: ::std::format!("{}", e),
                };
            }
            ::rudzio::suite::PhaseOutcome::Panicked(msg) => {
                break 'run ::rudzio::suite::TestOutcome::SetupFailed {
                    elapsed: start.elapsed(),
                    message: ::std::format!("panic: {}", msg),
                };
            }
            ::rudzio::suite::PhaseOutcome::Cancelled => {
                break 'run ::rudzio::suite::TestOutcome::Cancelled;
            }
            ::rudzio::suite::PhaseOutcome::TimedOut => {
                break 'run ::rudzio::suite::TestOutcome::SetupFailed {
                    elapsed: start.elapsed(),
                    message: ::std::string::String::from(
                        "setup timed out",
                    ),
                };
            }
            ::rudzio::suite::PhaseOutcome::Hung => {
                break 'run ::rudzio::suite::TestOutcome::Hung {
                    elapsed: start.elapsed(),
                };
            }
            _ => {
                break 'run ::rudzio::suite::TestOutcome::SetupFailed {
                    elapsed: start.elapsed(),
                    message: ::std::string::String::from(
                        "setup unknown outcome",
                    ),
                };
            }
        };
    }
}

/// Token stream for the per-test teardown phase: invokes
/// `Test::teardown` through the canonical phase helper with a fresh
/// unparented token, classifies the result into a `TeardownResult`,
/// and reports test-teardown failures.
fn run_test_teardown_phase_quote(runtime_type: &Path) -> TokenStream {
    quote! {
        // Per-test teardown wrapped in the phase helper
        // with a FRESH unparented token. Cleanup must run
        // to completion regardless of run-timeout / SIGINT
        // — that's the same guarantee we had before the
        // wrapper existed. Only the per-test-teardown
        // timeout (or the user cancelling the token they
        // received) can short-circuit it.
        let __rudzio_teardown_phase_token =
            ::rudzio::tokio_util::sync::CancellationToken::new();
        let __rudzio_teardown_phase =
            ::rudzio::suite::run_phase_with_timeout_and_cancel(
                ctx.teardown(__rudzio_teardown_phase_token.clone()),
                __rudzio_teardown_timeout,
                config.phase_hang_grace,
                __rudzio_teardown_phase_token,
                |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(rt, dur),
            ).await;
        let __rudzio_test_teardown_result = match __rudzio_teardown_phase {
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Ok(())) => {
                ::rudzio::suite::TeardownResult::Ok
            }
            ::rudzio::suite::PhaseOutcome::Completed(::std::result::Result::Err(e)) => {
                ::rudzio::suite::TeardownResult::Err(::std::format!("{}", e))
            }
            ::rudzio::suite::PhaseOutcome::Panicked(msg) => {
                ::rudzio::suite::TeardownResult::Panicked(msg)
            }
            ::rudzio::suite::PhaseOutcome::TimedOut => {
                ::rudzio::suite::TeardownResult::TimedOut
            }
            ::rudzio::suite::PhaseOutcome::Hung => {
                ::rudzio::suite::TeardownResult::Hung
            }
            ::rudzio::suite::PhaseOutcome::Cancelled => {
                ::rudzio::suite::TeardownResult::Err(
                    ::std::string::String::from("teardown cancelled"),
                )
            }
            _ => {
                ::rudzio::suite::TeardownResult::Err(
                    ::std::string::String::from("teardown unknown outcome"),
                )
            }
        };
        if !matches!(
            __rudzio_test_teardown_result,
            ::rudzio::suite::TeardownResult::Ok,
        ) {
            reporter.report_test_teardown_failure(
                token,
                <#runtime_type as ::rudzio::runtime::Runtime<'_>>::name(rt),
                __rudzio_test_teardown_result,
            );
        }
    }
}

/// Token stream for the per-test timeout-resolution preamble:
/// emits the three `Option<u64>` const overrides and the resolved
/// `Duration` lets that combine override → config-default →
/// unbounded.
fn run_test_timeouts_quote(
    attr_test_timeout_secs: &TokenStream,
    attr_setup_timeout_secs: &TokenStream,
    attr_teardown_timeout_secs: &TokenStream,
) -> TokenStream {
    quote! {
        // Per-test attribute overrides (None when the bare
        // `#[rudzio::test]` form is used). Resolution wins
        // attr → config default → unbounded.
        const __RUDZIO_OVERRIDE_TEST_TIMEOUT_SECS:
            ::core::option::Option<u64> = #attr_test_timeout_secs;
        const __RUDZIO_OVERRIDE_SETUP_TIMEOUT_SECS:
            ::core::option::Option<u64> = #attr_setup_timeout_secs;
        const __RUDZIO_OVERRIDE_TEARDOWN_TIMEOUT_SECS:
            ::core::option::Option<u64> = #attr_teardown_timeout_secs;
        let test_timeout: ::std::option::Option<::std::time::Duration> =
            __RUDZIO_OVERRIDE_TEST_TIMEOUT_SECS
                .map(::std::time::Duration::from_secs)
                .or(test_timeout);
        let __rudzio_setup_timeout: ::std::option::Option<::std::time::Duration> =
            __RUDZIO_OVERRIDE_SETUP_TIMEOUT_SECS
                .map(::std::time::Duration::from_secs)
                .or(config.test_setup_timeout);
        let __rudzio_teardown_timeout: ::std::option::Option<::std::time::Duration> =
            __RUDZIO_OVERRIDE_TEARDOWN_TIMEOUT_SECS
                .map(::std::time::Duration::from_secs)
                .or(config.test_teardown_timeout);
    }
}

/// Emit the per-(R, S) `RuntimeGroupOwner` impl plus the anchoring ZST
/// + `static` instance the runner indexes through.
///
/// Multiple suite blocks declaring the same `(runtime, suite, test)`
/// triple emit functionally equivalent owners; the runner picks any
/// one and ignores the rest.
fn runtime_group_owner_impl(args: &RuntimeGroupOwnerImplArgs<'_>) -> TokenStream {
    let &RuntimeGroupOwnerImplArgs {
        cfg_attrs,
        mod_name,
        owner_static,
        owner_struct,
        runtime_ctor,
        runtime_type,
        suite_base,
    } = args;
    let runtime_init = owner_runtime_init_quote(runtime_ctor, runtime_type, mod_name);
    let suite_setup = owner_suite_setup_quote(suite_base, runtime_type);
    let dispatch_loop = owner_dispatch_loop_quote(suite_base, runtime_type);
    let suite_teardown = owner_suite_teardown_quote(runtime_type);
    quote! {
        #(#cfg_attrs)*
        #[doc(hidden)]
        struct #owner_struct;

        #(#cfg_attrs)*
        #[doc(hidden)]
        static #owner_static: #owner_struct = #owner_struct;

        #(#cfg_attrs)*
        impl ::rudzio::suite::RuntimeGroupOwner for #owner_struct {
            #[inline]
            fn group_key(&self) -> ::rudzio::suite::RuntimeGroupKey {
                ::rudzio::suite::RuntimeGroupKey::of::<
                    #runtime_type,
                    #suite_base::<'static, #runtime_type>,
                >()
            }

            fn run_group(
                &self,
                req: ::rudzio::suite::RunRequest<'_>,
                reporter: &dyn ::rudzio::suite::Reporter,
            ) -> ::rudzio::suite::Summary {
                use ::rudzio::context::Suite as _;
                use ::rudzio::context::Test as _;
                use ::rudzio::runtime::Runtime as _;
                use ::rudzio::futures_util::FutureExt as _;
                use ::rudzio::futures_util::StreamExt as _;
                use ::rudzio::futures_util::stream::FuturesUnordered;

                #runtime_init

                // Step 3: drive the suite under the runtime's own block_on.
                // Borrows of `&rt` and `&suite` are scoped to this call;
                // every lifetime here is tied to the local stack frame.
                let async_summary: ::rudzio::suite::Summary =
                    ::rudzio::runtime::Runtime::block_on(&rt, async {
                        let mut summary = summary;

                        #suite_setup

                        #dispatch_loop

                        #suite_teardown

                        summary
                    });

                drop(rt);
                async_summary
            }
        }
    }
}

/// Convert a `snake_case` identifier to `UpperCamelCase`. Splits on `_`,
/// uppercases the first char of each segment, and drops the underscores
/// so the result conforms to Rust's type-name convention.
fn to_upper_camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for segment in name.split('_').filter(|seg| !seg.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// Stringify an ident into a `format_ident!`-safe fragment. Raw identifiers
/// (`r#type`) print with a leading `r#`, which is not a valid identifier
/// fragment; strip it so generated hidden names stay well-formed.
fn ident_fragment(ident: &Ident) -> String {
    ident
        .to_string()
        .strip_prefix("r#")
        .map_or_else(|| ident.to_string(), str::to_owned)
}

/// Generate the two hidden idents for a test fn: the token-static name
/// (`__RUDZIO_TOKEN_{MOD}_{TEST}_{cfg}`) and the run-test helper name
/// (`__rudzio_run_test_{mod}_{test}_{cfg}`). Raw test idents (`r#type`) are
/// sanitized so the fragments stay well-formed identifiers.
fn per_test_idents(mod_name: &Ident, test_name: &Ident, cfg_idx: usize) -> (Ident, Ident) {
    let test_fragment = ident_fragment(test_name);
    let token_static = format_ident!(
        "__RUDZIO_TOKEN_{}_{}_{}",
        mod_name.to_string().to_ascii_uppercase(),
        test_fragment.to_ascii_uppercase(),
        cfg_idx,
    );
    let run_test_fn = format_ident!(
        "__rudzio_run_test_{}_{}_{}",
        mod_name,
        test_fragment,
        cfg_idx
    );
    (token_static, run_test_fn)
}

/// Hidden const name holding the suite module's outer `module_path!()`.
/// Emitted once per suite at the outer scope so macro-generated test fns
/// (whose token statics live inside the module) can reference
/// `super::<this>` and report the same module path as direct fns.
fn suite_module_path_const(mod_name: &Ident) -> Ident {
    format_ident!(
        "__RUDZIO_SUITE_PATH_{}",
        mod_name.to_string().to_ascii_uppercase()
    )
}

/// Best-effort source line of a test fn's identifier for panic-hook
/// attribution. `proc_macro2::Span::start().line` returns compiler line
/// tracking in proc-macro context; a file with >2^32 lines is
/// pathological and surfaced as a hard `syn::Error` rather than silently
/// truncated.
fn source_line_of(test: &ItemFn) -> syn::Result<u32> {
    u32::try_from(test.sig.ident.span().start().line).map_err(|err| {
        syn::Error::new(
            test.sig.ident.span(),
            format!("source line exceeds u32::MAX: {err}"),
        )
    })
}

/// Site-dependent naming triple for a test fn: how the dispatch call reaches
/// the fn, how the token static references the owner, and how it derives the
/// module path for display.
fn registration_paths(
    site: RegistrationSite,
    mod_name: &Ident,
    test_name: &Ident,
    owner_static: &Ident,
) -> (TokenStream, TokenStream, TokenStream) {
    match site {
        RegistrationSite::Inner => {
            let suite_path_const = suite_module_path_const(mod_name);
            (
                quote! { #test_name },
                quote! { super::#owner_static },
                quote! { super::#suite_path_const },
            )
        }
        RegistrationSite::Outer => (
            quote! { #mod_name::#test_name },
            quote! { #owner_static },
            quote! { ::core::module_path!() },
        ),
    }
}

/// Parse a `name = <ident>,` field and return the identifier.
fn parse_named_ident(input: ParseStream<'_>, name: &str) -> syn::Result<Ident> {
    let key: Ident = input.parse()?;
    if key != name {
        return Err(syn::Error::new_spanned(key, format!("expected `{name}`")));
    }
    let _: Token![=] = input.parse()?;
    let value: Ident = input.parse()?;
    let _: Token![,] = input.parse()?;
    Ok(value)
}

/// Parse a `name = <integer>,` field and return the `u32` value.
fn parse_named_u32(input: ParseStream<'_>, name: &str) -> syn::Result<u32> {
    let key: Ident = input.parse()?;
    if key != name {
        return Err(syn::Error::new_spanned(key, format!("expected `{name}`")));
    }
    let _: Token![=] = input.parse()?;
    let value: LitInt = input.parse()?;
    let _: Token![,] = input.parse()?;
    value.base10_parse()
}

/// Parse a `name = <integer>,` field and return the `usize` value.
fn parse_named_usize(input: ParseStream<'_>, name: &str) -> syn::Result<usize> {
    let key: Ident = input.parse()?;
    if key != name {
        return Err(syn::Error::new_spanned(key, format!("expected `{name}`")));
    }
    let _: Token![=] = input.parse()?;
    let value: LitInt = input.parse()?;
    let _: Token![,] = input.parse()?;
    value.base10_parse()
}

/// Parse a `name = <path>,` field and return the path.
fn parse_named_path(input: ParseStream<'_>, name: &str) -> syn::Result<Path> {
    let key: Ident = input.parse()?;
    if key != name {
        return Err(syn::Error::new_spanned(key, format!("expected `{name}`")));
    }
    let _: Token![=] = input.parse()?;
    let value: Path = input.parse()?;
    let _: Token![,] = input.parse()?;
    Ok(value)
}

/// Parse a `name = <fn item>,` field and return the item (consuming the
/// trailing comma).
fn parse_named_item_fn(input: ParseStream<'_>, name: &str) -> syn::Result<ItemFn> {
    let key: Ident = input.parse()?;
    if key != name {
        return Err(syn::Error::new_spanned(key, format!("expected `{name}`")));
    }
    let _: Token![=] = input.parse()?;
    let value: ItemFn = input.parse()?;
    let _: Token![,] = input.parse()?;
    Ok(value)
}

/// Token-level entry point for the `#[rudzio::test]` attribute macro.
///
/// Runs *after* `macro_rules!` expansion on the concrete fn, so it sees test
/// fns the `#[rudzio::suite]` macro never does (opaque `Item::Macro` bodies).
/// It emits:
///   1. the fn with runtime generics applied and the test marker stripped, and
///   2. an invocation of the hidden `__rudzio_register_test!` macro the suite
///      emitted inside the module, forwarding the fn (marker re-attached) so
///      the per-config registration codegen can read `#[ignore]` /
///      `#[rudzio::test(..)]` arguments.
///
/// # Errors
///
/// `input` does not parse as an [`ItemFn`].
#[inline]
pub fn expand_test_attr(args: &TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let func: ItemFn = syn::parse2(input)?;
    let source_line = source_line_of(&func)?;

    // The attribute macro receives `input` with `#[rudzio::test]` (and its
    // argument tokens) already removed; re-attach the marker so the forwarded
    // fn retains the args the shared per-test codegen reads.
    let test_attr = if args.is_empty() {
        quote! { #[rudzio::test] }
    } else {
        quote! { #[rudzio::test(#args)] }
    };

    let mut emitted_fn = apply_runtime_generics(func.clone());
    strip_test_attrs(&mut emitted_fn);

    Ok(quote! {
        #emitted_fn

        __rudzio_register_test! { #source_line, #test_attr #func }
    })
}

/// Token-level entry point for the hidden `__register_test!` proc macro.
///
/// Given one `(runtime, suite)` config plus a concrete test fn, emits the
/// per-config `run_test_fn` + token static inside the module. The fn itself
/// is emitted once by `#[rudzio::test]`, so it never duplicates across configs.
#[inline]
pub fn expand_register_test(input: TokenStream) -> syn::Result<TokenStream> {
    let reg: RegisterTestInput = syn::parse2(input)?;

    let (helper, token_static) = generate_per_test(&GeneratePerTestArgs {
        cfg_attrs: &[],
        cfg_idx: reg.cfg_idx,
        mod_name: &reg.mod_name,
        owner_static: &reg.owner,
        runtime_type: &reg.runtime_type,
        suite_base: &reg.suite_base,
        site: RegistrationSite::Inner,
        source_line: reg.source_line,
        test: &reg.test,
    })?;

    Ok(quote! {
        #helper
        #token_static
    })
}

/// Build the hidden forwarding macro + module-path const for a suite whose
/// module body contains `macro_rules!` invocations. Each config becomes a
/// `#[cfg]`-gated `__register_test!` invocation so non-matching targets elide
/// that config's registration.
fn generate_macro_registration_support(
    mod_name: &Ident,
    args: &MainArgs,
) -> syn::Result<MacroRegistrationSupport> {
    let suite_path_const_ident = suite_module_path_const(mod_name);

    let mut config_arms = Vec::new();
    let mut type_aliases = Vec::new();
    for (cfg_idx, cfg) in args.configs.iter().enumerate() {
        let cfg_attrs = &cfg.attrs;
        let runtime_type = cfg.runtime_type()?;
        let suite_base = &cfg.suite;
        let owner_static = format_ident!(
            "__RUDZIO_OWNER_{}_{}",
            mod_name.to_string().to_ascii_uppercase(),
            cfg_idx,
        );
        // Aliases are emitted at the OUTER scope (siblings of the suite
        // module) and referenced from inside via `super::`. Two suites in the
        // same file both keying off `cfg_idx` alone would emit colliding
        // `__RudzioRuntime0` / `__RudzioSuite0` type aliases (E0428). Include
        // the module name so each suite's aliases are namespaced, matching the
        // `__RUDZIO_OWNER_{mod}_{cfg_idx}` owner-static convention. The name
        // goes through `to_upper_camel` so a lowercase module name (`tests`)
        // does not turn the alias non-camel-case.
        let mod_camel = to_upper_camel(&mod_name.to_string());
        let runtime_alias = format_ident!("__RudzioRuntime{}{}", mod_camel, cfg_idx);
        let suite_alias = format_ident!("__RudzioSuite{}{}", mod_camel, cfg_idx);

        // The registration items land inside the module, but the user writes
        // the runtime/suite paths at the outer scope (`runtime = Multithread::new`
        // with a crate-root `use`). Aliasing each type at the outer scope and
        // referencing `super::<alias>` from inside the module keeps both the
        // qualified and unqualified spellings resolving correctly. The suite
        // type is generic over `<'a, R>` (matching how the codegen applies it).
        type_aliases.push(quote! {
            #(#cfg_attrs)*
            #[doc(hidden)]
            type #runtime_alias = #runtime_type;
            #(#cfg_attrs)*
            #[doc(hidden)]
            type #suite_alias<'a, R> = #suite_base<'a, R>;
        });

        config_arms.push(quote! {
            #(#cfg_attrs)*
            ::rudzio::__register_test! {
                mod_name = #mod_name,
                source_line = $source_line,
                cfg_idx = #cfg_idx,
                runtime = super::#runtime_alias,
                suite = super::#suite_alias,
                owner = #owner_static,
                test = $($test)*,
            }
        });
    }

    let inner_macro_rules = quote! {
        #[allow(unused_macros)]
        #[doc(hidden)]
        macro_rules! __rudzio_register_test {
            ($source_line:literal, $($test:tt)*) => {
                #(#config_arms)*
            };
        }
    };

    let outer_items = quote! {
        #(#type_aliases)*
        #[doc(hidden)]
        #[allow(dead_code)]
        const #suite_path_const_ident: &'static str = ::core::module_path!();
    };

    Ok(MacroRegistrationSupport {
        inner_macro_rules,
        outer_items,
    })
}
