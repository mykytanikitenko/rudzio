use syn::{Attribute, ItemFn};

/// How a `#[rudzio::test]` fn relates to its per-test context.
///
/// Used by the suite codegen to decide how to dispatch the test body.
/// Regardless of variant, the runner always creates a fresh per-test
/// context (so suite `context`, test teardown, and cancel-token
/// propagation behave uniformly); the variant only controls whether —
/// and how — that context is passed to the test fn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CtxKind {
    /// `fn test(ctx: &mut T)` — mutable borrow.
    Mutable,
    /// `fn test()` — no context parameter. Setup + teardown still run;
    /// the test body just doesn't see the context.
    None,
    /// `fn test(ctx: &T)` — shared borrow.
    Shared,
}

/// Inspect `func`'s first parameter and classify it into a [`CtxKind`].
///
/// Non-reference and typed-self parameters (unusual in test fns but
/// possible) are treated as [`CtxKind::None`] — the codegen calls the
/// fn with no arguments in those cases, which will fail to compile
/// loudly rather than silently wire up a `&ctx`.
#[inline]
#[must_use]
pub fn classify_ctx_param(func: &ItemFn) -> CtxKind {
    let Some(first) = func.sig.inputs.first() else {
        return CtxKind::None;
    };
    let syn::FnArg::Typed(pat_type) = first else {
        return CtxKind::None;
    };
    let syn::Type::Reference(type_ref) = &*pat_type.ty else {
        return CtxKind::None;
    };
    if type_ref.mutability.is_some() {
        CtxKind::Mutable
    } else {
        CtxKind::Shared
    }
}

/// Returns `true` when `func` carries any attribute the suite macro
/// recognises as a test marker (`#[test]` or `#[rudzio::test]`).
#[inline]
#[must_use]
pub fn has_test_attr(func: &ItemFn) -> bool {
    func.attrs.iter().any(is_test_attr)
}

/// Returns `true` when `func` is declared `async fn`.
#[inline]
#[must_use]
pub const fn is_async_fn(func: &ItemFn) -> bool {
    func.sig.asyncness.is_some()
}

/// Returns `true` when `attr` is one of the test markers the suite
/// macro recognises (`#[test]` or `#[rudzio::test]`).
#[inline]
#[must_use]
pub fn is_test_attr(attr: &Attribute) -> bool {
    attr.path().is_ident("test")
        || (attr.path().segments.len() == 2
            && {
                let first = attr.path().segments.first();
                let second = attr.path().segments.last();
                matches!((first, second),
                (Some(first_seg), Some(second_seg)) if first_seg.ident == "rudzio" && second_seg.ident == "test")
            })
}

/// `true` if the function's return type is `()` — either left off
/// entirely (`fn foo()`) or written out (`fn foo() -> ()`).
///
/// Drives the `#[rudzio::test]` dispatch: void-return bodies get
/// wrapped in a trailing `Ok(())` so the surrounding `.map_err(...)`
/// chain still type-checks, letting users write bare libtest-shaped
/// tests without thinking about Result.
#[inline]
#[must_use]
pub fn returns_unit(func: &ItemFn) -> bool {
    match &func.sig.output {
        syn::ReturnType::Default => true,
        syn::ReturnType::Type(_, ty) => matches!(
            &**ty,
            syn::Type::Tuple(tuple) if tuple.elems.is_empty()
        ),
    }
}

/// Transform test function signature to add generic lifetimes and runtime type.
///
/// Converts: `async fn test(ctx: &BaseTestContext) -> Result<()>`
/// To:       `async fn test<'test_context, 'suite_context: 'test_context, R: Runtime<'suite_context> + Sync>(ctx: &'test_context BaseTestContext<'suite_context, R>) -> Result<()>`.
///
/// Two distinct lifetimes are emitted on purpose:
///   - `'suite_context` is the lifetime parameter on the user's context type (the
///     suite context tier). The runtime borrow inside the type lives
///     for `'suite_context`.
///   - `'test_context` is the per-test borrow lifetime — the duration of the
///     `&` (or `&mut`) the runner hands the test fn. It is strictly
///     `'suite_context: 'test_context` (the borrow can't outlive the value it borrows from).
///
/// Collapsing both into a single `'a` (as the previous implementation did)
/// forces the borrow to live as long as the type itself, which the borrow
/// checker then refuses to release before the post-test
/// `ctx.teardown()` move, breaking `&mut TestContext` test signatures.
///
/// If the context type already has generic arguments, leaves it unchanged.
#[inline]
#[must_use]
pub fn apply_runtime_generics(mut func: ItemFn) -> ItemFn {
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
                #type_path<'suite_context, R>
            };
            *pat_type.ty = syn::Type::Reference(syn::TypeReference {
                and_token: type_ref.and_token,
                lifetime: Some(syn::Lifetime::new("'test_context", Span::call_site())),
                mutability: type_ref.mutability,
                elem: Box::new(inner_type),
            });
        }

        func.sig.generics = syn::parse_quote! {
            <'test_context, 'suite_context: 'test_context, R: ::rudzio::runtime::Runtime<'suite_context> + ::std::marker::Sync>
        };
    }

    func
}
