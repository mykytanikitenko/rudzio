use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Expr, ItemFn, Meta};

use crate::transform::is_test_attr;

/// A `#[cfg_attr(predicate, ignore)]` attribute narrowed to the fields the
/// codegen needs: the predicate (re-emitted as `cfg!(predicate)`) and the
/// optional ignore reason.
struct CfgAttrIgnore {
    /// The cfg predicate to re-emit as `cfg!(#predicate)`.
    predicate: Meta,
    /// The optional reason string of the wrapped `#[ignore]`.
    reason: Option<String>,
}

impl CfgAttrIgnore {
    /// Extract `(predicate, ignore_reason)` from a `#[cfg_attr(...)]`
    /// attribute whose trailing attributes include an `ignore`. Returns
    /// `None` when the attribute is not a `cfg_attr` wrapping `ignore`.
    fn from_attr(attr: &Attribute) -> Option<Self> {
        let list = attr.parse_args::<CfgAttrList>().ok()?;
        for meta in &list.attrs {
            if meta.path().is_ident("ignore") {
                let reason = meta_ignore_reason(meta);
                return Some(Self {
                    predicate: list.predicate.clone(),
                    reason,
                });
            }
        }
        None
    }
}

/// `#[cfg_attr(predicate, attr1, attr2, ...)]` contents: the leading cfg
/// predicate plus the trailing attribute metas.
struct CfgAttrList {
    /// The trailing attributes (e.g. `ignore`) after the predicate.
    attrs: Vec<Meta>,
    /// The leading cfg predicate.
    predicate: Meta,
}

impl Parse for CfgAttrList {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let predicate: Meta = input.parse()?;
        let mut attrs = Vec::new();
        while !input.is_empty() {
            let _: syn::Token![,] = input.parse()?;
            if input.is_empty() {
                break;
            }
            attrs.push(input.parse::<Meta>()?);
        }
        Ok(Self { attrs, predicate })
    }
}

/// Parsed contents of a `#[rudzio::test(...)]` attribute.
///
/// All fields are `None` / `None` when the bare attribute form is used
/// (`#[rudzio::test]` with no arguments). Each `*_secs` slot is a
/// per-test override of the matching CLI default (`--test-timeout`,
/// `--test-setup-timeout`, `--test-teardown-timeout`); resolution is
/// `attr.or(config_default)` at runtime.
#[derive(Debug)]
#[non_exhaustive]
pub struct TestAttrArgs {
    pub benchmark: Option<Expr>,
    pub setup_timeout_secs: Option<u64>,
    pub teardown_timeout_secs: Option<u64>,
    pub timeout_secs: Option<u64>,
}

impl TestAttrArgs {
    /// Empty `#[rudzio::test(...)]` arguments — every slot `None`.
    /// Replaces `Default::default()` per P-1 (no `Default` impls in
    /// workspace-internal types). Used as the seed for
    /// `extract_test_attr_args`'s incremental fold.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            benchmark: None,
            setup_timeout_secs: None,
            teardown_timeout_secs: None,
            timeout_secs: None,
        }
    }
}

/// Back-compat thin wrapper: callers that only care about the
/// `benchmark = ...` slot can keep using this without unpacking the
/// full struct.
///
/// # Errors
///
/// Propagates any parse failure from [`extract_test_attr_args`] —
/// unknown attribute keywords or malformed values.
#[inline]
pub fn extract_benchmark_expr(func: &ItemFn) -> syn::Result<Option<Expr>> {
    extract_test_attr_args(func).map(|args| args.benchmark)
}

/// Extract the `#[ignore]` flag and optional reason string from a test fn,
/// including `#[cfg_attr(cond, ignore)]`-wrapped forms.
///
/// Accepts every plain form rustc accepts (`#[ignore]`, `#[ignore = "..."]`,
/// `#[ignore("...")]`, `#[ignore(reason = "...")]`) plus the same four forms
/// wrapped in `#[cfg_attr(cond, ...)]`. Returns token streams rather than
/// concrete values: a `cfg_attr`-wrapped ignore yields `cfg!(cond)` (and a
/// conditional reason), which the codegen embeds directly in the token
/// static's const initializer.
#[inline]
#[must_use]
pub fn extract_ignore_reason(func: &ItemFn) -> (TokenStream, TokenStream) {
    let mut ignored_terms: Vec<TokenStream> = Vec::new();
    let mut reason: Option<TokenStream> = None;

    for attr in &func.attrs {
        if attr.path().is_ident("ignore") {
            ignored_terms.push(quote! { true });
            if reason.is_none()
                && let Some(text) = meta_ignore_reason(&attr.meta)
            {
                reason = Some(quote! { #text });
            }
        }
        if attr.path().is_ident("cfg_attr")
            && let Some(cfg_attr_ignore) = CfgAttrIgnore::from_attr(attr)
        {
            let predicate = &cfg_attr_ignore.predicate;
            ignored_terms.push(quote! { cfg!(#predicate) });
            if reason.is_none()
                && let Some(text) = cfg_attr_ignore.reason
            {
                reason = Some(quote! { if cfg!(#predicate) { #text } else { "" } });
            }
        }
    }

    let ignored = ignored_terms
        .into_iter()
        .reduce(|acc, term| quote! { (#acc) || (#term) })
        .unwrap_or_else(|| quote! { false });
    let reason_expr = reason.unwrap_or_else(|| quote! { "" });

    (ignored, reason_expr)
}

/// Parse every `#[rudzio::test(...)]` attribute on `func` into a single
/// [`TestAttrArgs`].
///
/// The bare `#[rudzio::test]` form (no parens / no arguments) returns
/// the default-empty struct. Unknown keywords or malformed attribute
/// bodies surface as `Err(syn::Error)` so the compiler points straight
/// at the offending token instead of the macro losing the signal
/// silently.
///
/// # Errors
///
/// Returns `Err(syn::Error)` when an attribute body contains an
/// unknown keyword (anything other than `benchmark`, `timeout`,
/// `setup_timeout`, `teardown_timeout`) or when a value fails to
/// parse as the expected type (e.g. a non-integer literal in a
/// `*_timeout = ...` slot).
#[inline]
pub fn extract_test_attr_args(func: &ItemFn) -> syn::Result<TestAttrArgs> {
    let mut args = TestAttrArgs::empty();
    for attr in &func.attrs {
        if !is_test_attr(attr) {
            continue;
        }
        // Bare `#[rudzio::test]` → `Meta::Path`, no args. Nothing to do.
        if matches!(attr.meta, Meta::Path(_)) {
            continue;
        }
        parse_test_attr_args(attr, &mut args)?;
    }
    Ok(args)
}

/// The reason string of a single `#[ignore]`-shaped meta (`ignore`,
/// `ignore = "…"`, `ignore("…")`, `ignore(reason = "…")`). Returns `None` for
/// the bare `ignore` form.
fn meta_ignore_reason(meta: &Meta) -> Option<String> {
    match meta {
        Meta::Path(_) => None,
        Meta::NameValue(nv) => {
            if let Expr::Lit(expr_lit) = &nv.value
                && let syn::Lit::Str(lit_str) = &expr_lit.lit
            {
                Some(lit_str.value())
            } else {
                None
            }
        }
        Meta::List(list) => {
            if let Ok(lit) = syn::parse2::<syn::LitStr>(list.tokens.clone()) {
                return Some(lit.value());
            }
            let Ok(Meta::NameValue(nv)) = syn::parse2::<Meta>(list.tokens.clone()) else {
                return None;
            };
            if nv.path.is_ident("reason")
                && let Expr::Lit(expr_lit) = nv.value
                && let syn::Lit::Str(lit_str) = expr_lit.lit
            {
                return Some(lit_str.value());
            }
            None
        }
    }
}

/// Populate `args` from one `#[rudzio::test(...)]` attribute body.
///
/// Returns `Err(syn::Error)` when the body contains an unknown keyword
/// or a value that fails to parse as the expected type.
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
