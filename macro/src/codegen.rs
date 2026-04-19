use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Ident, Item, ItemFn, ItemMod, Path};

use crate::args::{MainArgs, RuntimeConfig};
use crate::transform::{has_test_attr, is_async_fn, is_test_attr, transform_test_signature};

pub(crate) fn expand_main(args: MainArgs, input_mod: ItemMod) -> TokenStream {
    let items = match &input_mod.content {
        Some((_brace, items)) => items.clone(),
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
                && has_test_attr(func) {
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
                    modified.attrs.retain(|attr| !is_test_attr(attr));
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

    let main_fn = generate_main(mod_name, &test_functions, &args.configs);

    let expanded = quote! {
        #(#mod_attrs)*
        #mod_vis mod #mod_name {
            #(#processed_items)*
        }

        #main_fn
    };

    expanded.into()
}

fn generate_main(
    mod_name: &Ident,
    tests: &[ItemFn],
    configs: &[RuntimeConfig],
) -> proc_macro2::TokenStream {
    let test_count = tests.len();

    let runtime_threads: Vec<_> = configs.iter().enumerate().map(|(idx, cfg)| {
        let runtime_ctor = &cfg.runtime;
        let global_type = cfg.global_type();
        let test_type = cfg.test_type();
        let runtime_name = quote!(#runtime_ctor).to_string();
        let handle_name = format_ident!("handle_{}", idx);

        let ignored_logs: Vec<_> = tests.iter().filter_map(|test| {
            let (ignored, ignore_reason) = extract_ignore_reason(test);
            if !ignored {
                return None;
            }
            let fn_name_str = test.sig.ident.to_string();
            Some(quote! {
                {
                    let test_name: &str = #fn_name_str;
                    if #ignore_reason.is_empty() {
                        ::tracing::info!(test_name, "IGNORED");
                    } else {
                        ::tracing::info!(test_name, reason = #ignore_reason, "IGNORED");
                    }
                    summary.ignored += 1;
                }
            })
        }).collect();

        let push_tasks: Vec<_> = tests.iter().filter_map(|test| {
            let (ignored, _) = extract_ignore_reason(test);
            if ignored {
                return None;
            }
            let fn_name = &test.sig.ident;
            let fn_name_str = fn_name.to_string();
            let is_async = is_async_fn(test);

            let run_body = if is_async {
                quote! {
                    let test_result = ::std::panic::AssertUnwindSafe(async {
                        #mod_name::#fn_name(&test_ctx).await
                    }).catch_unwind().await;
                }
            } else {
                quote! {
                    let test_result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                        #mod_name::#fn_name(&test_ctx)
                    }));
                }
            };

            Some(quote! {
                pending.push(::std::boxed::Box::pin({
                    let global = ::std::sync::Arc::clone(&global);
                    async move {
                        let test_name: &str = #fn_name_str;

                        let test_ctx: #test_type = match global.context().await {
                            Ok(ctx) => ctx,
                            Err(e) => {
                                ::tracing::error!(test_name, error = %e, "FAILED: failed to create test context");
                                return TestOutcome::Failed;
                            }
                        };

                        #run_body

                        if let Err(e) = test_ctx.teardown().await {
                            ::tracing::warn!(test_name, error = %e, "test teardown failed");
                        }

                        match test_result {
                            Ok(Ok(())) => {
                                ::tracing::info!(test_name, "PASSED");
                                TestOutcome::Passed
                            }
                            Ok(Err(e)) => {
                                ::tracing::error!(test_name, error = %e, "FAILED");
                                TestOutcome::Failed
                            }
                            Err(_) => {
                                ::tracing::error!(test_name, "PANICKED");
                                TestOutcome::Panicked
                            }
                        }
                    }
                }));
            })
        }).collect();

        quote! {
            let #handle_name = thread::spawn(move || {
                use ::rudzio::context::Global as _;
                use ::rudzio::context::Test as _;
                use ::rudzio::futures_util::FutureExt;
                use ::rudzio::futures_util::stream::{FuturesUnordered, StreamExt};

                let rt: &'static _ = match #runtime_ctor() {
                    Ok(runtime) => ::std::boxed::Box::leak(::std::boxed::Box::new(runtime)),
                    Err(e) => {
                        ::tracing::error!(runtime = #runtime_name, error = %e, "FATAL: failed to create runtime");
                        return Summary {
                            passed: 0,
                            failed: 0,
                            ignored: 0,
                            panicked: #test_count,
                        };
                    }
                };

                rt.block_on(async move {
                    let global: #global_type = match #global_type::setup(rt).await {
                        Ok(g) => g,
                        Err(e) => {
                            ::tracing::error!(runtime = #runtime_name, error = %e, "FATAL: failed to create global context");
                            return Summary {
                                passed: 0,
                                failed: 0,
                                ignored: 0,
                                panicked: #test_count,
                            };
                        }
                    };
                    let global = ::std::sync::Arc::new(global);

                    let mut summary = Summary::default();

                    #(#ignored_logs)*

                    let mut pending: ::std::vec::Vec<
                        ::std::pin::Pin<::std::boxed::Box<
                            dyn ::std::future::Future<Output = TestOutcome> + ::std::marker::Send
                        >>
                    > = ::std::vec::Vec::new();

                    #(#push_tasks)*

                    let threads = ::rudzio::runner::resolve_test_threads();
                    let mut in_flight: FuturesUnordered<
                        ::std::pin::Pin<::std::boxed::Box<
                            dyn ::std::future::Future<
                                Output = ::std::result::Result<TestOutcome, ::rudzio::runtime::JoinError>
                            > + ::std::marker::Send
                        >>
                    > = FuturesUnordered::new();
                    let mut queued = pending.into_iter();
                    for _ in 0..threads {
                        match queued.next() {
                            Some(fut) => in_flight.push(::std::boxed::Box::pin(rt.spawn(fut))),
                            None => break,
                        }
                    }
                    while let Some(result) = StreamExt::next(&mut in_flight).await {
                        let outcome = match result {
                            Ok(o) => o,
                            Err(_) => TestOutcome::Panicked,
                        };
                        match outcome {
                            TestOutcome::Passed => summary.passed += 1,
                            TestOutcome::Failed => summary.failed += 1,
                            TestOutcome::Panicked => summary.panicked += 1,
                        }
                        if let Some(fut) = queued.next() {
                            in_flight.push(::std::boxed::Box::pin(rt.spawn(fut)));
                        }
                    }

                    match ::std::sync::Arc::try_unwrap(global) {
                        Ok(g) => {
                            if let Err(e) = g.teardown().await {
                                ::tracing::warn!(runtime = #runtime_name, error = %e, "global teardown failed");
                            }
                        }
                        Err(_) => {
                            ::tracing::warn!(
                                runtime = #runtime_name,
                                "global teardown skipped: outstanding Arc references"
                            );
                        }
                    }

                    ::tracing::info!(
                        runtime = #runtime_name,
                        passed = summary.passed,
                        failed = summary.failed,
                        ignored = summary.ignored,
                        panicked = summary.panicked,
                        "runtime group complete"
                    );

                    summary
                })
            });
        }
    }).collect();

    let join_handles: Vec<_> = (0..configs.len())
        .map(|i| {
            let handle_name = format_ident!("handle_{}", i);
            quote! { #handle_name }
        })
        .collect();

    quote! {
        fn main() {
            use ::rudzio::runtime::Runtime;
            ::rudzio::init_tracing();

            use ::std::thread;

            #[derive(Debug, Default)]
            struct Summary {
                passed: usize,
                failed: usize,
                ignored: usize,
                panicked: usize,
            }

            #[derive(Debug, Clone, Copy)]
            enum TestOutcome {
                Passed,
                Failed,
                Panicked,
            }

            impl Summary {
                fn merge(&mut self, other: Summary) {
                    self.passed += other.passed;
                    self.failed += other.failed;
                    self.ignored += other.ignored;
                    self.panicked += other.panicked;
                }
            }

            #(#runtime_threads)*

            let mut total_summary = Summary::default();

            #(
                match #join_handles.join() {
                    Ok(summary) => total_summary.merge(summary),
                    Err(panic_payload) => {
                        let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                            s.to_string()
                        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic in runtime thread".to_string()
                        };
                        ::tracing::error!(panic = panic_msg, "runtime thread panicked");
                        total_summary.panicked += 1;
                    }
                }
            )*

            ::tracing::info!(
                passed = total_summary.passed,
                failed = total_summary.failed,
                ignored = total_summary.ignored,
                panicked = total_summary.panicked,
                total = #test_count,
                "test run complete"
            );

            let exit_code = if total_summary.failed > 0 || total_summary.panicked > 0 {
                1
            } else {
                0
            };

            std::process::exit(exit_code);
        }
    }
}

pub(crate) fn extract_ignore_reason(func: &ItemFn) -> (bool, String) {
    for attr in &func.attrs {
        if !attr.path().is_ident("ignore") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                return (true, s.value());
            }
        if matches!(attr.meta, syn::Meta::List(_)) {
            if let Ok(lit) = attr.parse_args::<syn::LitStr>() {
                return (true, lit.value());
            }
            if let Ok(syn::Meta::NameValue(nv)) = attr.parse_args::<syn::Meta>()
                && nv.path.is_ident("reason")
                    && let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) = nv.value
                    {
                        return (true, s.value());
                    }
        }
        return (true, String::new());
    }
    (false, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[::std::prelude::rust_2024::test]
    fn ignore_reason_none_for_fn_without_attr() {
        let func: ItemFn = parse_quote! {
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_eq!(extract_ignore_reason(&func), (false, String::new()));
    }

    #[::std::prelude::rust_2024::test]
    fn ignore_reason_bare_attribute() {
        let func: ItemFn = parse_quote! {
            #[ignore]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_eq!(extract_ignore_reason(&func), (true, String::new()));
    }

    #[::std::prelude::rust_2024::test]
    fn ignore_reason_name_value_form() {
        let func: ItemFn = parse_quote! {
            #[ignore = "because"]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_eq!(extract_ignore_reason(&func), (true, "because".to_owned()));
    }

    #[::std::prelude::rust_2024::test]
    fn ignore_reason_parenthesized_lit_form() {
        let func: ItemFn = parse_quote! {
            #[ignore("because")]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_eq!(extract_ignore_reason(&func), (true, "because".to_owned()));
    }

    #[::std::prelude::rust_2024::test]
    fn ignore_reason_parenthesized_named_form() {
        let func: ItemFn = parse_quote! {
            #[ignore(reason = "because")]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_eq!(extract_ignore_reason(&func), (true, "because".to_owned()));
    }
}
