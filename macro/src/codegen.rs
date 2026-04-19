use syn::ItemFn;

/// Extract the `#[ignore]` flag and optional reason string from a test fn.
///
/// Accepts every form rustc accepts (`#[ignore]`, `#[ignore = "..."]`,
/// `#[ignore("...")]`, `#[ignore(reason = "...")]`).
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
