//! `#[ignore]` attribute parser tests, dogfooded on rudzio.

use syn::{ItemFn, parse_quote};

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
use rudzio_macro_internals::codegen::extract_ignore_reason;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{ItemFn, Test, extract_ignore_reason, parse_quote};

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
