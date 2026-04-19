use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, Item, ItemFn, ItemMod, Path};
use syn::spanned::Spanned;

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

    let mut helper_fns: Vec<proc_macro2::TokenStream> = vec![];
    let mut token_statics: Vec<proc_macro2::TokenStream> = vec![];

    for (cfg_idx, cfg) in args.configs.iter().enumerate() {
        generate_per_config(
            mod_name,
            cfg_idx,
            cfg,
            &test_functions,
            &mut helper_fns,
            &mut token_statics,
        );
    }

    let expanded = quote! {
        #(#mod_attrs)*
        #mod_vis mod #mod_name {
            #(#processed_items)*
        }

        #(#helper_fns)*

        #(#token_statics)*
    };

    expanded.into()
}

fn generate_per_config(
    mod_name: &Ident,
    cfg_idx: usize,
    cfg: &RuntimeConfig,
    tests: &[ItemFn],
    helper_fns: &mut Vec<proc_macro2::TokenStream>,
    token_statics: &mut Vec<proc_macro2::TokenStream>,
) {
    let runtime_ctor = &cfg.runtime;
    let runtime_type = cfg.runtime_type();
    let global_base = &cfg.global;
    let test_base = &cfg.test;
    let runtime_name_str = quote!(#runtime_ctor).to_string();

    let make_runtime_fn =
        format_ident!("__rudzio_make_runtime_{}_{}", mod_name, cfg_idx);
    let group_id_fn =
        format_ident!("__rudzio_group_id_{}_{}", mod_name, cfg_idx);
    let make_global_fn =
        format_ident!("__rudzio_make_global_{}_{}", mod_name, cfg_idx);
    let teardown_global_fn =
        format_ident!("__rudzio_teardown_global_{}_{}", mod_name, cfg_idx);

    helper_fns.push(quote! {
        fn #make_runtime_fn()
            -> ::std::result::Result<
                ::std::boxed::Box<dyn ::rudzio::runtime::DynRuntime>,
                ::rudzio::test_case::BoxError,
            >
        {
            #runtime_ctor()
                .map(|rt| {
                    ::std::boxed::Box::new(rt)
                        as ::std::boxed::Box<dyn ::rudzio::runtime::DynRuntime>
                })
                .map_err(|e| ::rudzio::test_case::box_error(e))
        }

        fn #group_id_fn() -> ::std::any::TypeId {
            ::std::any::TypeId::of::<(#runtime_type, #global_base::<'static, #runtime_type>)>()
        }

        fn #make_global_fn(
            rt: &'static dyn ::rudzio::runtime::DynRuntime,
            cancel: ::rudzio::tokio_util::sync::CancellationToken,
        ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<
            Output = ::std::result::Result<
                ::std::boxed::Box<dyn ::std::any::Any + ::std::marker::Send + ::std::marker::Sync>,
                ::rudzio::test_case::BoxError,
            >
        > + ::std::marker::Send + 'static>> {
            use ::rudzio::context::Global as _;
            let rt = rt.as_any()
                .downcast_ref::<#runtime_type>()
                .expect("runtime type mismatch in make_global");
            ::std::boxed::Box::pin(async move {
                #global_base::<'static, #runtime_type>::setup(rt, cancel).await
                    .map(|g| {
                        ::std::boxed::Box::new(g)
                            as ::std::boxed::Box<
                                dyn ::std::any::Any
                                    + ::std::marker::Send
                                    + ::std::marker::Sync
                            >
                    })
                    .map_err(|e| ::rudzio::test_case::box_error(e))
            })
        }

        fn #teardown_global_fn(
            global: ::std::boxed::Box<
                dyn ::std::any::Any + ::std::marker::Send + ::std::marker::Sync
            >,
        ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<
            Output = ::std::result::Result<(), ::rudzio::test_case::BoxError>
        > + ::std::marker::Send + 'static>> {
            use ::rudzio::context::Global as _;
            let global = global
                .downcast::<#global_base::<'static, #runtime_type>>()
                .expect("global context type mismatch in teardown_global");
            ::std::boxed::Box::pin(async move {
                (*global).teardown().await
                    .map_err(|e| ::rudzio::test_case::box_error(e))
            })
        }
    });

    for test in tests {
        generate_per_test(
            mod_name,
            cfg_idx,
            &runtime_type,
            global_base,
            test_base,
            &runtime_name_str,
            &make_runtime_fn,
            &group_id_fn,
            &make_global_fn,
            &teardown_global_fn,
            test,
            helper_fns,
            token_statics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_per_test(
    mod_name: &Ident,
    cfg_idx: usize,
    runtime_type: &syn::Path,
    global_base: &syn::Path,
    test_base: &syn::Path,
    runtime_name_str: &str,
    make_runtime_fn: &Ident,
    group_id_fn: &Ident,
    make_global_fn: &Ident,
    teardown_global_fn: &Ident,
    test: &ItemFn,
    helper_fns: &mut Vec<proc_macro2::TokenStream>,
    token_statics: &mut Vec<proc_macro2::TokenStream>,
) {
    let test_name = &test.sig.ident;
    let test_name_str = test_name.to_string();
    let (ignored, ignore_reason) = extract_ignore_reason(test);
    let is_async = is_async_fn(test);

    // Extract source line so we can sort tokens into stable source order at
    // runtime (linkme doesn't preserve order).
    let source_line = test.sig.ident.span().unwrap().start().line() as u32;

    let make_test_ctx_fn =
        format_ident!("__rudzio_make_test_ctx_{}_{}_{}", mod_name, test_name, cfg_idx);
    let run_fn =
        format_ident!("__rudzio_run_{}_{}_{}", mod_name, test_name, cfg_idx);
    let teardown_test_fn =
        format_ident!("__rudzio_teardown_test_{}_{}_{}", mod_name, test_name, cfg_idx);
    let token_static = format_ident!(
        "__RUDZIO_TOKEN_{}_{}_{}",
        mod_name.to_string().to_ascii_uppercase(),
        test_name.to_string().to_ascii_uppercase(),
        cfg_idx,
    );

    let run_body = if is_async {
        quote! {
            #mod_name::#test_name(ctx).await
                .map_err(|e| ::rudzio::test_case::box_error(e))
        }
    } else {
        // Sync tests: call directly inside the async block so panics propagate
        // to the catch_unwind in spawn_test, where they are counted as Panicked.
        quote! {
            #mod_name::#test_name(ctx)
                .map_err(|e| ::rudzio::test_case::box_error(e))
        }
    };

    helper_fns.push(quote! {
        fn #make_test_ctx_fn(
            global: &'static (dyn ::std::any::Any + ::std::marker::Send + ::std::marker::Sync),
            cancel: ::rudzio::tokio_util::sync::CancellationToken,
        ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<
            Output = ::std::result::Result<
                ::std::boxed::Box<dyn ::std::any::Any + ::std::marker::Send>,
                ::rudzio::test_case::BoxError,
            >
        > + ::std::marker::Send + 'static>> {
            use ::rudzio::context::Global as _;
            let global = (global as &dyn ::std::any::Any)
                .downcast_ref::<#global_base::<'static, #runtime_type>>()
                .expect("global context type mismatch in make_test_ctx");
            ::std::boxed::Box::pin(async move {
                global.context(cancel).await
                    .map(|ctx: #test_base::<'static, #runtime_type>| {
                        ::std::boxed::Box::new(ctx)
                            as ::std::boxed::Box<dyn ::std::any::Any + ::std::marker::Send>
                    })
                    .map_err(|e| ::rudzio::test_case::box_error(e))
            })
        }

        fn #run_fn(
            ctx: &'static mut (dyn ::std::any::Any + ::std::marker::Send),
        ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<
            Output = ::std::result::Result<(), ::rudzio::test_case::BoxError>
        > + ::std::marker::Send + 'static>> {
            let ctx = ctx
                .downcast_mut::<#test_base::<'static, #runtime_type>>()
                .expect("test context type mismatch in run");
            ::std::boxed::Box::pin(async move { #run_body })
        }

        fn #teardown_test_fn(
            ctx: ::std::boxed::Box<dyn ::std::any::Any + ::std::marker::Send>,
        ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<
            Output = ::std::result::Result<(), ::rudzio::test_case::BoxError>
        > + ::std::marker::Send + 'static>> {
            use ::rudzio::context::Test as _;
            let ctx = ctx
                .downcast::<#test_base::<'static, #runtime_type>>()
                .expect("test context type mismatch in teardown_test");
            ::std::boxed::Box::pin(async move {
                (*ctx).teardown().await
                    .map_err(|e| ::rudzio::test_case::box_error(e))
            })
        }
    });

    token_statics.push(quote! {
        #[::rudzio::linkme::distributed_slice(::rudzio::TEST_TOKENS)]
        #[linkme(crate = ::rudzio::linkme)]
        static #token_static: ::rudzio::token::TestToken = ::rudzio::token::TestToken {
            name: #test_name_str,
            ignored: #ignored,
            ignore_reason: #ignore_reason,
            file: ::std::file!(),
            line: #source_line,
            runtime_group: #group_id_fn,
            runtime_name: #runtime_name_str,
            make_runtime: #make_runtime_fn,
            make_global: #make_global_fn,
            make_test_ctx: #make_test_ctx_fn,
            run: #run_fn,
            teardown_test: #teardown_test_fn,
            teardown_global: #teardown_global_fn,
        };
    });
}
