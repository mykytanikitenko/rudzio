use syn::{Attribute, ItemFn};

pub(crate) fn has_test_attr(func: &ItemFn) -> bool {
    func.attrs.iter().any(is_test_attr)
}

pub(crate) fn is_test_attr(attr: &Attribute) -> bool {
    attr.path().is_ident("test")
        || (attr.path().segments.len() == 2 && {
            let first = attr.path().segments.first();
            let second = attr.path().segments.last();
            matches!((first, second),
                (Some(f), Some(s)) if f.ident == "rudzio" && s.ident == "test")
        })
}

pub(crate) fn is_async_fn(func: &ItemFn) -> bool {
    func.sig.asyncness.is_some()
}

/// Returns `true` if `func`'s first parameter is `&mut T` (in any form).
///
/// Used by the suite codegen to decide whether to dispatch the test with
/// `&mut ctx` (and bind the per-test context as `let mut ctx = …`).
pub(crate) fn first_param_is_mut_ref(func: &ItemFn) -> bool {
    let Some(first) = func.sig.inputs.first() else {
        return false;
    };
    let syn::FnArg::Typed(pat_type) = first else {
        return false;
    };
    let syn::Type::Reference(type_ref) = &*pat_type.ty else {
        return false;
    };
    type_ref.mutability.is_some()
}

/// Transform test function signature to add generic lifetimes and runtime type.
///
/// Converts: `async fn test(ctx: &BaseTestContext) -> Result<()>`
/// To:       `async fn test<'tc, 'cg: 'tc, R: Runtime<'cg> + Sync>(ctx: &'tc BaseTestContext<'cg, R>) -> Result<()>`
///
/// Two distinct lifetimes are emitted on purpose:
///   - `'cg` is the lifetime parameter on the user's context type (the
///     "global context" tier). The runtime borrow inside the type lives
///     for `'cg`.
///   - `'tc` is the per-test borrow lifetime — the duration of the
///     `&` (or `&mut`) the runner hands the test fn. It is strictly
///     `'cg: 'tc` (the borrow can't outlive the value it borrows from).
///
/// Collapsing both into a single `'a` (as the previous implementation did)
/// forces the borrow to live as long as the type itself, which the borrow
/// checker then refuses to release before the post-test
/// `ctx.teardown()` move, breaking `&mut TestContext` test signatures.
///
/// If the context type already has generic arguments, leaves it unchanged.
pub(crate) fn transform_test_signature(mut func: ItemFn) -> ItemFn {
    use proc_macro2::Span;

    let should_transform = if let Some(first_param) = func.sig.inputs.first()
        && let syn::FnArg::Typed(pat_type) = first_param
        && let syn::Type::Reference(type_ref) = &*pat_type.ty
        && let syn::Type::Path(type_path) = &*type_ref.elem
        && let Some(last_seg) = type_path.path.segments.last()
    {
        last_seg.arguments.is_empty()
    } else {
        false
    };

    if should_transform {
        if let Some(first_param) = func.sig.inputs.first_mut()
            && let syn::FnArg::Typed(pat_type) = first_param
            && let syn::Type::Reference(type_ref) = &*pat_type.ty
            && let syn::Type::Path(type_path) = &*type_ref.elem
        {
            let inner_type: syn::Type = syn::parse_quote! {
                #type_path<'cg, R>
            };
            *pat_type.ty = syn::Type::Reference(syn::TypeReference {
                and_token: type_ref.and_token,
                lifetime: Some(syn::Lifetime::new("'tc", Span::call_site())),
                mutability: type_ref.mutability,
                elem: Box::new(inner_type),
            });
        }

        func.sig.generics = syn::parse_quote! {
            <'tc, 'cg: 'tc, R: ::rudzio::runtime::Runtime<'cg> + ::std::marker::Sync>
        };
    }

    func
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[::std::prelude::rust_2024::test]
    fn test_transform_simple_base_context() {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: &BaseTestContext) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = transform_test_signature(func);

        assert!(!transformed.sig.generics.params.is_empty());

        if let syn::FnArg::Typed(pat_type) = transformed.sig.inputs.first().unwrap() {
            if let syn::Type::Reference(type_ref) = &*pat_type.ty {
                assert!(type_ref.lifetime.is_some());
                if let syn::Type::Path(type_path) = &*type_ref.elem {
                    let last_seg = type_path.path.segments.last().unwrap();
                    assert!(!last_seg.arguments.is_empty());
                } else {
                    panic!("Expected TypePath");
                }
            } else {
                panic!("Expected Reference type");
            }
        } else {
            panic!("Expected Typed arg");
        }
    }

    #[::std::prelude::rust_2024::test]
    fn test_skip_already_generic_context() {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: &PostgresTestContext<'_, TokioMultiThread, MT_RUNTIME, NO_TLS>) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = transform_test_signature(func);

        assert!(transformed.sig.generics.params.is_empty());

        if let syn::FnArg::Typed(pat_type) = transformed.sig.inputs.first().unwrap() {
            if let syn::Type::Reference(type_ref) = &*pat_type.ty {
                if let syn::Type::Path(type_path) = &*type_ref.elem {
                    let last_seg = type_path.path.segments.last().unwrap();
                    assert!(!last_seg.arguments.is_empty());
                    let original: syn::TypePath =
                        parse_quote!(PostgresTestContext<'_, TokioMultiThread, MT_RUNTIME, NO_TLS>);
                    assert_eq!(type_path.path.segments.len(), original.path.segments.len());
                } else {
                    panic!("Expected TypePath");
                }
            } else {
                panic!("Expected Reference type");
            }
        } else {
            panic!("Expected Typed arg");
        }
    }

    #[::std::prelude::rust_2024::test]
    fn test_transform_custom_context() {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: &MyCustomContext) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = transform_test_signature(func);

        assert!(!transformed.sig.generics.params.is_empty());

        if let syn::FnArg::Typed(pat_type) = transformed.sig.inputs.first().unwrap() {
            if let syn::Type::Reference(type_ref) = &*pat_type.ty {
                if let syn::Type::Path(type_path) = &*type_ref.elem {
                    let last_seg = type_path.path.segments.last().unwrap();
                    assert_eq!(last_seg.ident.to_string(), "MyCustomContext");
                    assert!(!last_seg.arguments.is_empty());
                } else {
                    panic!("Expected TypePath");
                }
            } else {
                panic!("Expected Reference type");
            }
        } else {
            panic!("Expected Typed arg");
        }
    }

    #[::std::prelude::rust_2024::test]
    fn test_no_transform_non_reference_param() {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: BaseTestContext) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = transform_test_signature(func);

        assert!(transformed.sig.generics.params.is_empty());
    }

    #[::std::prelude::rust_2024::test]
    fn test_no_transform_no_params() {
        let func: ItemFn = parse_quote! {
            async fn test() -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = transform_test_signature(func);

        assert!(transformed.sig.generics.params.is_empty());
    }
}
