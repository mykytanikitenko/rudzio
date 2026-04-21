//! `#[ignore]` attribute parser tests, dogfooded on rudzio.

use syn::{ItemFn, parse_quote};

use rudzio_macro_internals::codegen::extract_ignore_reason;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::{ItemFn, extract_ignore_reason, parse_quote};
    use rudzio::common::context::Test;

    #[rudzio::test]
    fn ignore_reason_none_for_fn_without_attr(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        anyhow::ensure!(extract_ignore_reason(&func) == (false, String::new()));
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_bare_attribute(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        anyhow::ensure!(extract_ignore_reason(&func) == (true, String::new()));
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_name_value_form(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore = "because"]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        anyhow::ensure!(extract_ignore_reason(&func) == (true, "because".to_owned()));
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_parenthesized_lit_form(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore("because")]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        anyhow::ensure!(extract_ignore_reason(&func) == (true, "because".to_owned()));
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_parenthesized_named_form(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore(reason = "because")]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        anyhow::ensure!(extract_ignore_reason(&func) == (true, "because".to_owned()));
        Ok(())
    }
}
