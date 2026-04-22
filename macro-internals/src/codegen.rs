use syn::{Attribute, Expr, ItemFn};

use crate::transform::is_test_attr;

/// Extract the `benchmark = <expr>` argument from a `#[rudzio::test(...)]`
/// attribute, if one is present.
///
/// The bare `#[rudzio::test]` form (no parens / no arguments) returns
/// `Ok(None)`. A `#[rudzio::test(benchmark = expr)]` form returns
/// `Ok(Some(expr))`. Unknown keywords or malformed attribute bodies
/// surface as `Err(syn::Error)` so the compiler points straight at the
/// offending token instead of the macro losing the signal silently.
pub fn extract_benchmark_expr(func: &ItemFn) -> syn::Result<Option<Expr>> {
    let mut found: Option<Expr> = None;
    for attr in &func.attrs {
        if !is_test_attr(attr) {
            continue;
        }
        // Bare `#[rudzio::test]` → `Meta::Path`, no args. Nothing to do.
        if matches!(attr.meta, syn::Meta::Path(_)) {
            continue;
        }
        parse_test_attr_args(attr, &mut found)?;
    }
    Ok(found)
}

fn parse_test_attr_args(attr: &Attribute, found: &mut Option<Expr>) -> syn::Result<()> {
    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("benchmark") {
            let value = meta.value()?;
            let expr: Expr = value.parse()?;
            *found = Some(expr);
            Ok(())
        } else {
            Err(meta.error(
                "unknown argument to `#[rudzio::test]`; \
                 expected `benchmark = <strategy-expression>`",
            ))
        }
    })
}

/// Extract the `#[ignore]` flag and optional reason string from a test fn.
///
/// Accepts every form rustc accepts (`#[ignore]`, `#[ignore = "..."]`,
/// `#[ignore("...")]`, `#[ignore(reason = "...")]`).
pub fn extract_ignore_reason(func: &ItemFn) -> (bool, String) {
    for attr in &func.attrs {
        if !attr.path().is_ident("ignore") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta
            && let Expr::Lit(syn::ExprLit {
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
                && let Expr::Lit(syn::ExprLit {
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
