use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Ident, Item, ItemFn, ItemMod, Path};

use crate::args::{MainArgs, RuntimeConfig};
use crate::codegen::extract_ignore_reason;
use crate::transform::{has_test_attr, is_async_fn, is_test_attr, transform_test_signature};

pub(crate) fn expand_suite(args: MainArgs, input_mod: ItemMod) -> TokenStream {
    let items = match &input_mod.content {
        Some((_, items)) => items.clone(),
        None => {
            return syn::Error::new_spanned(input_mod, "expected module body, found empty module")
                .to_compile_error()
                .into()
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
                    pub_token: syn::token::Pub(func.span()),
                    paren_token: syn::token::Paren(func.span()),
                    in_token: None,
                    path: Box::new(Path::from(syn::PathSegment::from(Ident::new(
                        "super",
                        func.span(),
                    )))),
                });
                modified.attrs.retain(|a| !is_test_attr(a));
                modified = transform_test_signature(modified);
                return Item::Fn(modified);
            }
            item.clone()
        })
        .collect();

    let test_functions: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(func) if has_test_attr(func) => Some(func.clone()),
            _ => None,
        })
        .collect();

    if test_functions.is_empty() {
        return syn::Error::new_spanned(
            input_mod,
            "no test functions found in module - add functions with #[rudzio::test] attribute",
        )
        .to_compile_error()
        .into();
    }

    let mut helper_items: Vec<proc_macro2::TokenStream> = vec![];
    let mut token_statics: Vec<proc_macro2::TokenStream> = vec![];

    for (cfg_idx, cfg) in args.configs.iter().enumerate() {
        generate_per_config(
            mod_name,
            cfg_idx,
            cfg,
            &test_functions,
            &mut helper_items,
            &mut token_statics,
        );
    }

    let expanded = quote! {
        #(#mod_attrs)*
        #mod_vis mod #mod_name {
            #(#processed_items)*
        }

        #(#helper_items)*

        #(#token_statics)*
    };

    expanded.into()
}

fn generate_per_config(
    mod_name: &Ident,
    cfg_idx: usize,
    cfg: &RuntimeConfig,
    tests: &[ItemFn],
    helper_items: &mut Vec<proc_macro2::TokenStream>,
    token_statics: &mut Vec<proc_macro2::TokenStream>,
) {
    let runtime_ctor = &cfg.runtime;
    let runtime_type = cfg.runtime_type();
    let global_base = &cfg.global;
    let test_base = &cfg.test;
    let runtime_name_str = quote!(#runtime_ctor).to_string();

    let suite_id_struct =
        format_ident!("__RudzioSuiteId_{}_{}", mod_name, cfg_idx);
    let suite_struct = format_ident!("__RudzioSuite_{}_{}", mod_name, cfg_idx);
    let suite_static = format_ident!(
        "__RUDZIO_SUITE_{}_{}",
        mod_name.to_string().to_ascii_uppercase(),
        cfg_idx,
    );
    let run_one_fn = format_ident!("__rudzio_run_one_{}_{}", mod_name, cfg_idx);

    // Build the per-test dispatch arms (matched on suite_local_index).
    let dispatch_arms: Vec<_> = tests
        .iter()
        .enumerate()
        .map(|(idx, test)| {
            let test_name = &test.sig.ident;
            let is_async = is_async_fn(test);
            let call = if is_async {
                quote! {
                    #mod_name::#test_name(__rudzio_ctx).await
                        .map_err(|e| ::rudzio::test_case::box_error(e))
                }
            } else {
                quote! {
                    #mod_name::#test_name(__rudzio_ctx)
                        .map_err(|e| ::rudzio::test_case::box_error(e))
                }
            };
            quote! { #idx => { #call } }
        })
        .collect();

    // Suite-level types and static instance.
    helper_items.push(quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        struct #suite_id_struct;

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        struct #suite_struct;

        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #suite_static: #suite_struct = #suite_struct;
    });

    // The per-suite SuiteRunner impl.
    helper_items.push(quote! {
        impl ::rudzio::suite::SuiteRunner for #suite_struct {
            #[inline]
            fn suite_id(&self) -> ::rudzio::suite::SuiteId {
                ::rudzio::suite::SuiteId(::std::any::TypeId::of::<#suite_id_struct>())
            }

            #[inline]
            fn runtime_name(&self) -> &'static str {
                #runtime_name_str
            }

            fn run_suite(
                &self,
                req: ::rudzio::suite::SuiteRunRequest<'_>,
                reporter: &dyn ::rudzio::suite::SuiteReporter,
            ) -> ::rudzio::suite::SuiteSummary {
                use ::rudzio::context::Global as _;
                use ::rudzio::context::Test as _;
                use ::rudzio::runtime::Runtime as _;
                use ::rudzio::futures_util::FutureExt as _;
                use ::rudzio::futures_util::StreamExt as _;
                use ::rudzio::futures_util::stream::FuturesUnordered;

                const RUNTIME_NAME: &str = #runtime_name_str;

                // Step 1: create the runtime as a local value. The runtime
                // lives until the end of `run_suite`; nothing here is leaked
                // to `'static`.
                let rt = match #runtime_ctor() {
                    Ok(r) => r,
                    Err(e) => {
                        let msg = ::std::format!(
                            "FATAL: failed to create runtime [{}]: {}",
                            RUNTIME_NAME, e,
                        );
                        reporter.report_warning(&msg);
                        let mut summary = ::rudzio::suite::SuiteSummary::zero();
                        summary.total = req.tokens.len();
                        for tok in req.tokens {
                            reporter.report_outcome(
                                *tok,
                                RUNTIME_NAME,
                                ::rudzio::suite::TestOutcome::Panicked {
                                    elapsed: ::std::time::Duration::ZERO,
                                },
                            );
                            summary.panicked += 1;
                        }
                        return summary;
                    }
                };

                // Step 2: classify tokens into ignored vs active. Ignored
                // tests are reported eagerly so the listing order matches
                // libtest's behaviour.
                let mut summary = ::rudzio::suite::SuiteSummary::zero();
                summary.total = req.tokens.len();
                let mut active: ::std::vec::Vec<&'static ::rudzio::token::TestToken> =
                    ::std::vec::Vec::with_capacity(req.tokens.len());
                for tok in req.tokens {
                    let skip = match req.run_ignored {
                        ::rudzio::suite::RunIgnoredMode::Normal => tok.ignored,
                        ::rudzio::suite::RunIgnoredMode::Only
                        | ::rudzio::suite::RunIgnoredMode::Include => false,
                    };
                    if skip {
                        reporter.report_ignored(*tok, RUNTIME_NAME);
                        summary.ignored += 1;
                    } else {
                        active.push(*tok);
                    }
                }

                // Step 3: drive the suite to completion under the runtime's
                // own block_on. Borrows of `&rt` are scoped to this call;
                // every lifetime here is tied to the local stack frame.
                let async_summary: ::rudzio::suite::SuiteSummary =
                    ::rudzio::runtime::Runtime::block_on(&rt, async {
                        let mut summary = summary;

                        let global = match <
                            #global_base::<'_, #runtime_type> as ::rudzio::context::Global<'_, #runtime_type>
                        >::setup(&rt, req.root_token.clone()).await {
                            Ok(g) => g,
                            Err(e) => {
                                let msg = ::std::format!(
                                    "FATAL: failed to create global context [{}]: {}",
                                    RUNTIME_NAME, e,
                                );
                                reporter.report_warning(&msg);
                                for tok in active.iter() {
                                    reporter.report_outcome(
                                        *tok,
                                        RUNTIME_NAME,
                                        ::rudzio::suite::TestOutcome::Panicked {
                                            elapsed: ::std::time::Duration::ZERO,
                                        },
                                    );
                                    summary.panicked += 1;
                                }
                                return summary;
                            }
                        };

                        let mut in_flight = FuturesUnordered::new();
                        let mut queued = active.into_iter();

                        if !req.root_token.is_cancelled() {
                            for _ in 0..req.threads {
                                let ::std::option::Option::Some(tok) = queued.next() else { break };
                                in_flight.push(#run_one_fn(
                                    &global,
                                    tok,
                                    req.test_timeout,
                                    req.root_token.clone(),
                                    &rt,
                                    reporter,
                                ));
                            }
                        }

                        while let ::std::option::Option::Some((tok, outcome)) = in_flight.next().await {
                            match &outcome {
                                ::rudzio::suite::TestOutcome::Passed { .. } => summary.passed += 1,
                                ::rudzio::suite::TestOutcome::Failed { .. } => summary.failed += 1,
                                ::rudzio::suite::TestOutcome::Panicked { .. } => summary.panicked += 1,
                                ::rudzio::suite::TestOutcome::TimedOut => summary.timed_out += 1,
                                ::rudzio::suite::TestOutcome::Cancelled => summary.cancelled += 1,
                            }
                            reporter.report_outcome(tok, RUNTIME_NAME, outcome);
                            if !req.root_token.is_cancelled()
                                && let ::std::option::Option::Some(next) = queued.next()
                            {
                                in_flight.push(#run_one_fn(
                                    &global,
                                    next,
                                    req.test_timeout,
                                    req.root_token.clone(),
                                    &rt,
                                    reporter,
                                ));
                            }
                        }

                        for skipped in queued {
                            reporter.report_cancelled(skipped, RUNTIME_NAME);
                            summary.cancelled += 1;
                        }

                        // Drop in_flight before consuming global; the borrows
                        // that the (now-empty) FuturesUnordered held against
                        // `&global` would otherwise still be considered live.
                        ::std::mem::drop(in_flight);

                        match ::std::panic::AssertUnwindSafe(global.teardown())
                            .catch_unwind()
                            .await
                        {
                            ::std::result::Result::Ok(::std::result::Result::Ok(())) => {}
                            ::std::result::Result::Ok(::std::result::Result::Err(e)) => {
                                reporter.report_warning(&::std::format!(
                                    "global teardown failed [{}]: {}",
                                    RUNTIME_NAME, e,
                                ));
                            }
                            ::std::result::Result::Err(_) => {
                                reporter.report_warning(&::std::format!(
                                    "global teardown panicked [{}]",
                                    RUNTIME_NAME,
                                ));
                            }
                        }

                        summary
                    });

                drop(rt);
                async_summary
            }
        }
    });

    // The per-test orchestrator. Free async fn so its return type stays
    // consistent across all `in_flight.push(...)` calls.
    helper_items.push(quote! {
        #[doc(hidden)]
        #[allow(non_snake_case)]
        async fn #run_one_fn<'g>(
            global: &'g #global_base::<'g, #runtime_type>,
            token: &'static ::rudzio::token::TestToken,
            test_timeout: ::std::option::Option<::std::time::Duration>,
            root_token: ::rudzio::tokio_util::sync::CancellationToken,
            rt: &'g #runtime_type,
            reporter: &'g dyn ::rudzio::suite::SuiteReporter,
        ) -> (
            &'static ::rudzio::token::TestToken,
            ::rudzio::suite::TestOutcome,
        ) {
            use ::rudzio::context::Global as _;
            use ::rudzio::context::Test as _;
            use ::rudzio::runtime::Runtime as _;
            use ::rudzio::futures_util::FutureExt as _;

            let start = ::std::time::Instant::now();
            let per_test_token = root_token.child_token();

            let ctx = match global.context(per_test_token.clone()).await {
                ::std::result::Result::Ok(c) => c,
                ::std::result::Result::Err(e) => {
                    return (
                        token,
                        ::rudzio::suite::TestOutcome::Failed {
                            elapsed: start.elapsed(),
                            message: ::std::format!("failed to create test context: {}", e),
                        },
                    );
                }
            };

            let test_outcome = {
                let test_fut = async {
                    let __rudzio_ctx: &#test_base::<'_, #runtime_type> = &ctx;
                    match token.suite_local_index {
                        #(#dispatch_arms),*,
                        _ => ::std::panic!(
                            "rudzio internal error: suite_local_index out of range"
                        ),
                    }
                };

                let cancel_for_helper = per_test_token.clone();
                ::rudzio::suite::run_test_with_timeout_and_cancel(
                    test_fut,
                    test_timeout,
                    cancel_for_helper,
                    |dur| <#runtime_type as ::rudzio::runtime::Runtime<'_>>::sleep(rt, dur),
                ).await
            };

            let outcome = ::rudzio::suite::fill_elapsed(test_outcome, start.elapsed());

            let name = token.name;
            match ::std::panic::AssertUnwindSafe(ctx.teardown())
                .catch_unwind()
                .await
            {
                ::std::result::Result::Ok(::std::result::Result::Ok(())) => {}
                ::std::result::Result::Ok(::std::result::Result::Err(e)) => {
                    reporter.report_warning(&::std::format!(
                        "test teardown failed [{}]: {}",
                        name, e,
                    ));
                }
                ::std::result::Result::Err(_) => {
                    reporter.report_warning(&::std::format!(
                        "test teardown panicked [{}]",
                        name,
                    ));
                }
            }

            (token, outcome)
        }
    });

    // One TestToken static per test, all pointing at the same suite static.
    for (idx, test) in tests.iter().enumerate() {
        let test_name = &test.sig.ident;
        let test_name_str = test_name.to_string();
        let (ignored, ignore_reason) = extract_ignore_reason(test);
        let source_line = test.sig.ident.span().unwrap().start().line() as u32;
        let token_static = format_ident!(
            "__RUDZIO_TOKEN_{}_{}_{}",
            mod_name.to_string().to_ascii_uppercase(),
            test_name.to_string().to_ascii_uppercase(),
            cfg_idx,
        );

        token_statics.push(quote! {
            #[::rudzio::linkme::distributed_slice(::rudzio::token::TEST_TOKENS)]
            #[linkme(crate = ::rudzio::linkme)]
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static #token_static: ::rudzio::token::TestToken = ::rudzio::token::TestToken {
                name: #test_name_str,
                ignored: #ignored,
                ignore_reason: #ignore_reason,
                file: ::std::file!(),
                line: #source_line,
                runtime_name: #runtime_name_str,
                suite_runner: &#suite_static,
                suite_local_index: #idx,
            };
        });
    }
}
