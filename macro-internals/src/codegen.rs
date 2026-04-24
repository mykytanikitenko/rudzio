use syn::ItemFn;

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
