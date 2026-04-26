use syn::{Attribute, Expr, ItemFn};

use crate::transform::is_test_attr;

/// Parsed contents of a `#[rudzio::test(...)]` attribute.
///
/// All fields are `None` / `None` when the bare attribute form is used
/// (`#[rudzio::test]` with no arguments). Each `*_secs` slot is a
/// per-test override of the matching CLI default (`--test-timeout`,
/// `--test-setup-timeout`, `--test-teardown-timeout`); resolution is
/// `attr.or(config_default)` at runtime.
#[derive(Debug, Default)]
pub struct TestAttrArgs {
    pub benchmark: Option<Expr>,
    pub timeout_secs: Option<u64>,
    pub setup_timeout_secs: Option<u64>,
    pub teardown_timeout_secs: Option<u64>,
}

/// Parse every `#[rudzio::test(...)]` attribute on `func` into a single
/// [`TestAttrArgs`].
///
/// The bare `#[rudzio::test]` form (no parens / no arguments) returns
/// the default-empty struct. Unknown keywords or malformed attribute
/// bodies surface as `Err(syn::Error)` so the compiler points straight
/// at the offending token instead of the macro losing the signal
/// silently.
pub fn extract_test_attr_args(func: &ItemFn) -> syn::Result<TestAttrArgs> {
    let mut args = TestAttrArgs::default();
    for attr in &func.attrs {
        if !is_test_attr(attr) {
            continue;
        }
        // Bare `#[rudzio::test]` → `Meta::Path`, no args. Nothing to do.
        if matches!(attr.meta, syn::Meta::Path(_)) {
            continue;
        }
        parse_test_attr_args(attr, &mut args)?;
    }
    Ok(args)
}

/// Back-compat thin wrapper: callers that only care about the
/// `benchmark = ...` slot can keep using this without unpacking the
/// full struct.
pub fn extract_benchmark_expr(func: &ItemFn) -> syn::Result<Option<Expr>> {
    extract_test_attr_args(func).map(|a| a.benchmark)
}

fn parse_test_attr_args(attr: &Attribute, args: &mut TestAttrArgs) -> syn::Result<()> {
    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("benchmark") {
            let value = meta.value()?;
            let expr: Expr = value.parse()?;
            args.benchmark = Some(expr);
            Ok(())
        } else if meta.path.is_ident("timeout") {
            let value = meta.value()?;
            let lit: syn::LitInt = value.parse()?;
            args.timeout_secs = Some(lit.base10_parse::<u64>()?);
            Ok(())
        } else if meta.path.is_ident("setup_timeout") {
            let value = meta.value()?;
            let lit: syn::LitInt = value.parse()?;
            args.setup_timeout_secs = Some(lit.base10_parse::<u64>()?);
            Ok(())
        } else if meta.path.is_ident("teardown_timeout") {
            let value = meta.value()?;
            let lit: syn::LitInt = value.parse()?;
            args.teardown_timeout_secs = Some(lit.base10_parse::<u64>()?);
            Ok(())
        } else {
            Err(meta.error(
                "unknown argument to `#[rudzio::test]`; \
                 expected one of `benchmark = <strategy-expression>`, \
                 `timeout = <secs>`, `setup_timeout = <secs>`, \
                 `teardown_timeout = <secs>`",
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
