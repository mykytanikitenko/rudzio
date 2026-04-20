use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Ident, Item, ItemFn, ItemMod, Path};

use crate::args::{MainArgs, RuntimeConfig};
use crate::codegen::extract_ignore_reason;
use crate::transform::{
    first_param_is_mut_ref, has_test_attr, is_async_fn, is_test_attr, transform_test_signature,
};

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
    let _test_base = &cfg.test;
    let runtime_name_str = quote!(#runtime_ctor).to_string();

    // Stable id derived from the (runtime_type, global_base) path strings.
    // Two suite blocks declaring the same (R, G) get the same key and share
    // an OS thread / runtime / global at runtime.
    let group_key_source = format!(
        "{}::{}",
        quote!(#runtime_type),
        quote!(#global_base),
    );

    let mod_camel = to_upper_camel(&mod_name.to_string());
    let owner_struct = format_ident!("__RudzioOwner{}{}", mod_camel, cfg_idx);
    let owner_static = format_ident!(
        "__RUDZIO_OWNER_{}_{}",
        mod_name.to_string().to_ascii_uppercase(),
        cfg_idx,
    );

    // Suite-level owner ZST + static instance.
    helper_items.push(quote! {
        #[doc(hidden)]
        struct #owner_struct;

        #[doc(hidden)]
        static #owner_static: #owner_struct = #owner_struct;
    });

    // The per-(R, G) RuntimeGroupOwner impl. Multiple suite blocks sharing
    // (R, G) all emit functionally equivalent owners; the runner picks any
    // one and ignores the rest.
    helper_items.push(quote! {
        impl ::rudzio::suite::RuntimeGroupOwner for #owner_struct {
            #[inline]
            fn group_key(&self) -> ::rudzio::suite::RuntimeGroupKey {
                ::rudzio::suite::RuntimeGroupKey(
                    ::rudzio::suite::fnv1a64(#group_key_source),
                )
            }

            #[inline]
            fn runtime_name(&self) -> &'static str {
                #runtime_name_str
            }

            fn run_group(
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

                // Step 1: create the runtime as a local value. Lives until
                // the end of `run_group`; nothing leaked to `'static`.
                let rt: #runtime_type = match #runtime_ctor() {
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

                // Step 2: classify ignored vs active.
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

                // Step 3: drive the suite under the runtime's own block_on.
                // Borrows of `&rt` and `&global` are scoped to this call;
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

                        // Pointers we hand to each per-test fn pointer. The
                        // pointed-to types match the token's group_key —
                        // guaranteed by the macro that emits both sides.
                        let runtime_ptr: *const () = (&rt as *const #runtime_type).cast::<()>();
                        let global_ptr: *const () =
                            (&global as *const #global_base::<'_, #runtime_type>).cast::<()>();

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
                                    global_ptr,
                                    ::std::marker::PhantomData,
                                    tok,
                                    req.test_timeout,
                                    req.root_token.clone(),
                                    reporter,
                                )
                            }
                        };

                        if !req.root_token.is_cancelled() {
                            for _ in 0..req.threads {
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
                                ::rudzio::suite::TestOutcome::Failed { .. } => summary.failed += 1,
                                ::rudzio::suite::TestOutcome::Panicked { .. } => summary.panicked += 1,
                                ::rudzio::suite::TestOutcome::TimedOut => summary.timed_out += 1,
                                ::rudzio::suite::TestOutcome::Cancelled => summary.cancelled += 1,
                            }
                            reporter.report_outcome(tok, RUNTIME_NAME, outcome);
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
                            reporter.report_cancelled(skipped, RUNTIME_NAME);
                            summary.cancelled += 1;
                        }

                        // Drop in_flight before consuming global; the now-empty
                        // FuturesUnordered would otherwise still be considered
                        // a live borrow.
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

    // One per-test HRTB unsafe fn pointer + one TestToken static per test.
    for test in tests {
        let test_name = &test.sig.ident;
        let test_name_str = test_name.to_string();
        let (ignored, ignore_reason) = extract_ignore_reason(test);
        let source_line = test.sig.ident.span().unwrap().start().line() as u32;
        let is_async = is_async_fn(test);
        let wants_mut = first_param_is_mut_ref(test);

        let token_static = format_ident!(
            "__RUDZIO_TOKEN_{}_{}_{}",
            mod_name.to_string().to_ascii_uppercase(),
            test_name.to_string().to_ascii_uppercase(),
            cfg_idx,
        );
        let run_test_fn = format_ident!(
            "__rudzio_run_test_{}_{}_{}",
            mod_name, test_name, cfg_idx,
        );

        // Inline `&ctx` / `&mut ctx` into the call site so no intermediate
        // `let __rudzio_ctx = ...` binding is captured by the test_fut
        // coroutine — that binding made NLL pessimistically extend the
        // borrow past the .await, blocking the subsequent ctx.teardown().
        let dispatch_arg = if wants_mut {
            quote! { &mut ctx }
        } else {
            quote! { &ctx }
        };

        let ctx_binding = if wants_mut {
            quote! { let mut ctx }
        } else {
            quote! { let ctx }
        };

        let dispatch_call = if is_async {
            quote! {
                #mod_name::#test_name(#dispatch_arg).await
                    .map_err(|e| ::rudzio::test_case::box_error(e))
            }
        } else {
            quote! {
                #mod_name::#test_name(#dispatch_arg)
                    .map_err(|e| ::rudzio::test_case::box_error(e))
            }
        };

        helper_items.push(quote! {
            #[doc(hidden)]
            unsafe fn #run_test_fn<'g>(
                runtime_ptr: *const (),
                global_ptr: *const (),
                _phantom: ::std::marker::PhantomData<&'g ()>,
                token: &'static ::rudzio::token::TestToken,
                test_timeout: ::std::option::Option<::std::time::Duration>,
                root_token: ::rudzio::tokio_util::sync::CancellationToken,
                reporter: &'g dyn ::rudzio::suite::SuiteReporter,
            ) -> ::std::pin::Pin<::std::boxed::Box<
                dyn ::std::future::Future<Output = ::rudzio::suite::TestOutcome> + 'g
            >> {
                use ::rudzio::context::Global as _;
                use ::rudzio::context::Test as _;
                use ::rudzio::runtime::Runtime as _;
                use ::rudzio::futures_util::FutureExt as _;

                ::std::boxed::Box::pin(async move {
                    // SAFETY: caller (the runtime group owner) hands us
                    // pointers whose `runtime_group_key` matches this fn's;
                    // the macro emitted both sides, so the concrete types
                    // are `#runtime_type` and `#global_base::<'g, …>`.
                    #[allow(unsafe_code)]
                    let rt: &'g #runtime_type =
                        unsafe { &*(runtime_ptr as *const #runtime_type) };
                    #[allow(unsafe_code)]
                    let global: &'g #global_base::<'g, #runtime_type> = unsafe {
                        &*(global_ptr as *const #global_base::<'g, #runtime_type>)
                    };

                    let start = ::std::time::Instant::now();
                    let per_test_token = root_token.child_token();

                    #ctx_binding = match global.context(per_test_token.clone()).await {
                        ::std::result::Result::Ok(c) => c,
                        ::std::result::Result::Err(e) => {
                            return ::rudzio::suite::TestOutcome::Failed {
                                elapsed: start.elapsed(),
                                message: ::std::format!(
                                    "failed to create test context: {}", e,
                                ),
                            };
                        }
                    };

                    let test_outcome = {
                        let test_fut = async {
                            #dispatch_call
                        };

                        ::rudzio::suite::run_test_with_timeout_and_cancel(
                            test_fut,
                            test_timeout,
                            per_test_token.clone(),
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

                    outcome
                })
            }
        });

        token_statics.push(quote! {
            #[::rudzio::linkme::distributed_slice(::rudzio::token::TEST_TOKENS)]
            #[linkme(crate = ::rudzio::linkme)]
            #[doc(hidden)]
            static #token_static: ::rudzio::token::TestToken = ::rudzio::token::TestToken {
                name: #test_name_str,
                ignored: #ignored,
                ignore_reason: #ignore_reason,
                file: ::std::file!(),
                line: #source_line,
                runtime_name: #runtime_name_str,
                runtime_group_key: ::rudzio::suite::RuntimeGroupKey(
                    ::rudzio::suite::fnv1a64(#group_key_source),
                ),
                runtime_group_owner: &#owner_static,
                run_test: #run_test_fn,
            };
        });
    }
}

/// Convert a snake_case identifier to UpperCamelCase. Splits on `_`,
/// uppercases the first char of each segment, and drops the underscores
/// so the result conforms to Rust's type-name convention.
fn to_upper_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for segment in s.split('_').filter(|seg| !seg.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}
