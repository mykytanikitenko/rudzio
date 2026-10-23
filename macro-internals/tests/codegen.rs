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

    /// `extract_ignore_reason` returns `(ignored, reason)` token streams;
    /// assert on their string forms.
    fn assert_ignore(func: &ItemFn, ignored: &str, reason: &str) {
        let (ignored_tokens, reason_tokens) = extract_ignore_reason(func);
        assert_eq!(ignored_tokens.to_string(), ignored, "ignored tokens");
        assert_eq!(reason_tokens.to_string(), reason, "reason tokens");
    }

    #[rudzio::test]
    fn ignore_reason_none_for_fn_without_attr(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_ignore(&func, "false", "\"\"");
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_bare_attribute(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_ignore(&func, "true", "\"\"");
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_name_value_form(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore = "because"]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_ignore(&func, "true", "\"because\"");
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_parenthesized_lit_form(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore("because")]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_ignore(&func, "true", "\"because\"");
        Ok(())
    }

    #[rudzio::test]
    fn ignore_reason_parenthesized_named_form(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[ignore(reason = "because")]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        assert_ignore(&func, "true", "\"because\"");
        Ok(())
    }

    /// A `#[cfg_attr(cond, ignore)]` must be detected: the ignored flag is
    /// emitted as `cfg!(cond)` and the reason (when present) is conditional.
    #[rudzio::test]
    fn cfg_attr_ignore_is_emitted_as_cfg_predicate(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[cfg_attr(all(), ignore)]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let (ignored, reason) = extract_ignore_reason(&func);
        let ignored_str = ignored.to_string();
        anyhow::ensure!(
            ignored_str.contains("cfg"),
            "expected cfg! predicate: {ignored_str}"
        );
        anyhow::ensure!(
            ignored_str.contains("all"),
            "expected all() predicate: {ignored_str}"
        );
        anyhow::ensure!(
            reason.to_string() == "\"\"",
            "expected empty reason: {reason}"
        );
        Ok(())
    }

    /// A `#[cfg_attr(cond, ignore = "…")]` emits a conditional reason.
    #[rudzio::test]
    fn cfg_attr_ignore_reason_is_conditional(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[cfg_attr(all(), ignore = "because")]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let (ignored, reason) = extract_ignore_reason(&func);
        let ignored_str = ignored.to_string();
        let reason_str = reason.to_string();
        anyhow::ensure!(
            ignored_str.contains("cfg"),
            "expected cfg! predicate: {ignored_str}"
        );
        anyhow::ensure!(
            reason_str.contains("because"),
            "expected reason in {reason_str}"
        );
        anyhow::ensure!(
            reason_str.contains("cfg"),
            "expected conditional in {reason_str}"
        );
        Ok(())
    }
}
