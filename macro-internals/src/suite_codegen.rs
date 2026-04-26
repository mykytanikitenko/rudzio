use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::token::{Paren, Pub};
use syn::{Expr, Ident, Item, ItemFn, ItemMod, Path};

use crate::parse::{MainArgs, RuntimeConfig};
use crate::codegen::{TestAttrArgs, extract_ignore_reason, extract_test_attr_args};
use crate::transform::{
    CtxKind, classify_ctx_param, has_test_attr, is_async_fn, is_test_attr, apply_runtime_generics,
};

/// Bundle of inputs for [`generate_per_test`]; gathered into a struct so
/// the helper does not exceed clippy's `too_many_arguments` threshold.
struct GeneratePerTestArgs<'args> {
    /// Index of the parent runtime-config block within the suite macro.
    cfg_idx: usize,
    /// Stable string keying the runtime group; matches the owner's key.
    group_key_source: &'args str,
    /// Identifier of the wrapping `mod tests` block.
    mod_name: &'args Ident,
    /// Identifier of the per-config `RuntimeGroupOwner` static instance.
    owner_static: &'args Ident,
    /// Concrete runtime type for this group.
    runtime_type: &'args Path,
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

/// Convert each per-test attribute timeout (`timeout_secs`,
/// `setup_timeout_secs`, `teardown_timeout_secs`) into an
/// `Option<u64>` const expression token stream. The runtime-side
/// resolver then computes
/// `override.map(Duration::from_secs).or(config.<phase>_timeout)`.
fn attr_timeout_overrides(
    attr_args: &TestAttrArgs,
) -> (TokenStream, TokenStream, TokenStream) {
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
    mod_name: &Ident,
    test_name: &Ident,
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
                #mod_name::#test_name(#dispatch_call_args).await.into_rudzio_result()
            }
        }
    } else {
        quote! {
            {
                use ::rudzio::IntoRudzioResult as _;
                #mod_name::#test_name(#dispatch_call_args).into_rudzio_result()
            }
        }
    };
    let bench_body_closure = if is_async {
        quote! {
            || async {
                use ::rudzio::IntoRudzioResult as _;
                #mod_name::#test_name(#dispatch_call_args).await.into_rudzio_result()
            }
        }
    } else {
        quote! {
            || async {
                use ::rudzio::IntoRudzioResult as _;
                #mod_name::#test_name(#dispatch_call_args).into_rudzio_result()
            }
        }
    };
    (dispatch_call, bench_body_closure)
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

    let mod_attrs = &input_mod.attrs;
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

    if test_functions.is_empty() {
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

    let expanded = quote! {
        #(#mod_attrs)*
        #mod_vis mod #mod_name {
            #(#processed_items)*
        }

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
    let runtime_ctor = &cfg.runtime;
    let runtime_type = cfg.runtime_type();
    let suite_base = &cfg.suite;

    // Stable id derived from the (runtime_type, suite_base) path strings.
    // Two suite blocks declaring the same (R, S) get the same key and share
    // an OS thread / runtime / suite at runtime.
    let group_key_source = format!("{}::{}", quote!(#runtime_type), quote!(#suite_base));

    let mod_camel = to_upper_camel(&mod_name.to_string());
    let owner_struct = format_ident!("__RudzioOwner{}{}", mod_camel, cfg_idx);
    let owner_static = format_ident!(
        "__RUDZIO_OWNER_{}_{}",
        mod_name.to_string().to_ascii_uppercase(),
        cfg_idx,
    );

    helper_items.push(runtime_group_owner_impl(
        &owner_struct,
        &owner_static,
        runtime_ctor,
        &runtime_type,
        suite_base,
        mod_name,
        &group_key_source,
    ));

    for test in tests {
        let (helper, token_static) = generate_per_test(&GeneratePerTestArgs {
            cfg_idx,
            group_key_source: &group_key_source,
            mod_name,
            owner_static: &owner_static,
            runtime_type: &runtime_type,
            suite_base,
            test,
        })?;
        helper_items.push(helper);
        token_statics.push(token_static);
    }

    Ok(())
}

/// Emit the `(helper_item, token_static)` pair for a single
/// `#[rudzio::test]` fn. Pulled out of [`generate_per_config`]'s loop body
/// to keep that fn readable and within clippy's per-fn line budget.
fn generate_per_test(args: &GeneratePerTestArgs<'_>) -> syn::Result<(TokenStream, TokenStream)> {
    let &GeneratePerTestArgs {
        cfg_idx,
        group_key_source,
        mod_name,
        owner_static,
        runtime_type,
        suite_base,
        test,
    } = args;

    let test_name = &test.sig.ident;
    let test_name_str = test_name.to_string();
    let (ignored, ignore_reason) = extract_ignore_reason(test);
    let attr_args = extract_test_attr_args(test)?;
    let (attr_test_timeout_secs, attr_setup_timeout_secs, attr_teardown_timeout_secs) =
        attr_timeout_overrides(&attr_args);
    let benchmark = attr_args.benchmark;
    let has_benchmark = benchmark.is_some();
    // `proc_macro2::Span::start().line` returns line tracking from the
    // compiler in proc-macro context and from proc-macro2's own
    // tracking (e.g. `syn::parse_str`) in regular contexts. Any file
    // with >2^32 lines is pathological; we surface overflow as a hard
    // `syn::Error` rather than silently truncating.
    let source_line = u32::try_from(test.sig.ident.span().start().line).map_err(|err| {
        syn::Error::new(
            test.sig.ident.span(),
            format!("source line exceeds u32::MAX: {err}"),
        )
    })?;
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

    let token_static_ident = format_ident!(
        "__RUDZIO_TOKEN_{}_{}_{}",
        mod_name.to_string().to_ascii_uppercase(),
        test_name.to_string().to_ascii_uppercase(),
        cfg_idx,
    );
    let run_test_fn = format_ident!("__rudzio_run_test_{}_{}_{}", mod_name, test_name, cfg_idx,);

    // `ctx` is bound in every branch because the runner always runs
    // per-test teardown (`ctx.teardown()`) — that's the whole point
    // of the `Test` trait, regardless of whether the test body
    // takes a context parameter.
    let ctx_binding = match ctx_kind {
        CtxKind::Mutable => quote! { let mut ctx },
        CtxKind::Shared | CtxKind::None => quote! { let ctx },
    };

    let (dispatch_call, bench_body_closure) =
        dispatch_call_quotes(mod_name, test_name, ctx_kind, is_async);

    let test_outcome_expr = benchmark.as_ref().map_or_else(
        || plain_test_outcome(&dispatch_call, runtime_type),
        |bench_expr| bench_test_outcome(bench_expr, &dispatch_call, runtime_type, &bench_body_closure),
    );

    let helper_item = run_test_fn_quote(&RunTestFnArgs {
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
        #[::rudzio::linkme::distributed_slice(::rudzio::token::TEST_TOKENS)]
        #[linkme(crate = ::rudzio::linkme)]
        #[doc(hidden)]
        static #token_static_ident: ::rudzio::token::TestToken = ::rudzio::token::TestToken {
            name: #test_name_str,
            module_path: ::core::module_path!(),
            ignored: #ignored,
            ignore_reason: #ignore_reason,
            has_benchmark: #has_benchmark,
            file: ::std::file!(),
            line: #source_line,
            runtime_group_key: ::rudzio::suite::RuntimeGroupKey(
                ::rudzio::suite::fnv1a64(#group_key_source),
            ),
            runtime_group_owner: &#owner_static,
            run_test: #run_test_fn,
        };
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

        let dispatch = |tok: &'static ::rudzio::token::TestToken| {
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
                            &'static ::rudzio::token::TestToken,
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
            }
            reporter.report_outcome(tok, runtime_name, outcome);
            if !req.root_token.is_cancelled()
                && let ::std::option::Option::Some(next) = queued.next()
            {
                let fut: ::std::pin::Pin<::std::boxed::Box<
                    dyn ::std::future::Future<
                        Output = (
                            &'static ::rudzio::token::TestToken,
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
                let mut summary = ::rudzio::suite::SuiteSummary::zero();
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
        let mut summary = ::rudzio::suite::SuiteSummary::zero();
        summary.total = req.tokens.len();
        let mut active: ::std::vec::Vec<&'static ::rudzio::token::TestToken> =
            ::std::vec::Vec::with_capacity(req.tokens.len());
        for tok in req.tokens {
            let ignore_skip = match req.config.run_ignored {
                ::rudzio::config::RunIgnoredMode::Normal => tok.ignored,
                ::rudzio::config::RunIgnoredMode::Only
                | ::rudzio::config::RunIgnoredMode::Include => false,
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
fn owner_suite_setup_drain_quote(
    outcome: &TokenStream,
    counter_field: &Ident,
) -> TokenStream {
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
                let __rudzio_err_msg = ::std::format!("{}", e);
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
                ::rudzio::suite::TeardownResult::Err(::std::format!("{}", e))
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
        #[doc(hidden)]
        unsafe fn #run_test_fn<'s>(
            runtime_ptr: *const (),
            suite_ptr: *const (),
            _phantom: ::std::marker::PhantomData<&'s ()>,
            token: &'static ::rudzio::token::TestToken,
            test_timeout: ::std::option::Option<::std::time::Duration>,
            root_token: ::rudzio::tokio_util::sync::CancellationToken,
            reporter: &'s dyn ::rudzio::suite::SuiteReporter,
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
                // attributed to the right TestId.
                let __rudzio_hardlimit_guard = config.acquire_hardlimit_permit();

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
fn runtime_group_owner_impl(
    owner_struct: &Ident,
    owner_static: &Ident,
    runtime_ctor: &Path,
    runtime_type: &Path,
    suite_base: &Path,
    mod_name: &Ident,
    group_key_source: &str,
) -> TokenStream {
    let runtime_init = owner_runtime_init_quote(runtime_ctor, runtime_type, mod_name);
    let suite_setup = owner_suite_setup_quote(suite_base, runtime_type);
    let dispatch_loop = owner_dispatch_loop_quote(suite_base, runtime_type);
    let suite_teardown = owner_suite_teardown_quote(runtime_type);
    quote! {
        #[doc(hidden)]
        struct #owner_struct;

        #[doc(hidden)]
        static #owner_static: #owner_struct = #owner_struct;

        impl ::rudzio::suite::RuntimeGroupOwner for #owner_struct {
            #[inline]
            fn group_key(&self) -> ::rudzio::suite::RuntimeGroupKey {
                ::rudzio::suite::RuntimeGroupKey(
                    ::rudzio::suite::fnv1a64(#group_key_source),
                )
            }

            fn run_group(
                &self,
                req: ::rudzio::suite::SuiteRunRequest<'_>,
                reporter: &dyn ::rudzio::suite::SuiteReporter,
            ) -> ::rudzio::suite::SuiteSummary {
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
                let async_summary: ::rudzio::suite::SuiteSummary =
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
