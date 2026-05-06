//! Argument-parser tests for `rudzio-macro-internals`. Dogfooded: every
//! test body runs under rudzio's own runner.

use quote::ToTokens;

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy};
use rudzio_macro_internals::parse::RuntimeConfig;

/// Render any [`ToTokens`] value to a whitespace-stripped string —
/// the test suite compares parsed paths by their canonical token
/// shape rather than by structural equality on `syn` AST nodes.
fn render(tokens: &impl ToTokens) -> String {
    tokens.to_token_stream().to_string().replace(' ', "")
}

/// Parse `source` as a [`RuntimeConfig`] and return the error
/// message string when it fails. Bails when parsing unexpectedly
/// succeeds — the rejection cases must surface a `syn::Error`
/// pointing at the offending token rather than silently accepting
/// the input.
fn parse_err_msg(source: &str) -> anyhow::Result<String> {
    match syn::parse_str::<RuntimeConfig>(source) {
        Ok(_) => anyhow::bail!("expected parse to fail for `{source}`"),
        Err(err) => Ok(err.to_string()),
    }
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{RuntimeConfig, Test, parse_err_msg, render};

    #[rudzio::test]
    fn parses_suite_and_test_keywords(_ctx: &Test) -> anyhow::Result<()> {
        let config: RuntimeConfig = syn::parse_str(
            "( runtime = rudzio::runtime::tokio::Multithread::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test )",
        )?;
        anyhow::ensure!(
            render(&config.runtime) == "rudzio::runtime::tokio::Multithread::new",
            "runtime rendered as `{}`",
            render(&config.runtime),
        );
        anyhow::ensure!(render(&config.suite) == "rudzio::common::context::Suite");
        anyhow::ensure!(render(&config.test) == "rudzio::common::context::Test");
        Ok(())
    }

    #[rudzio::test]
    fn parser_accepts_trailing_comma_after_test(_ctx: &Test) -> anyhow::Result<()> {
        let _parsed: RuntimeConfig = syn::parse_str(
            "( runtime = rudzio::runtime::tokio::Multithread::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test, )",
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn rejects_unknown_suite_keyword(_ctx: &Test) -> anyhow::Result<()> {
        let msg = parse_err_msg(
            "( runtime = rudzio::runtime::tokio::Multithread::new, suit = rudzio::common::context::Suite, test = rudzio::common::context::Test )",
        )?;
        anyhow::ensure!(
            msg.contains("suite"),
            "error should mention the `suite` keyword, got: {msg}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn rejects_unknown_test_keyword(_ctx: &Test) -> anyhow::Result<()> {
        let msg = parse_err_msg(
            "( runtime = rudzio::runtime::tokio::Multithread::new, suite = rudzio::common::context::Suite, tst = rudzio::common::context::Test )",
        )?;
        anyhow::ensure!(
            msg.contains("test"),
            "error should mention the `test` keyword, got: {msg}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn rejects_turbofish_on_suite(_ctx: &Test) -> anyhow::Result<()> {
        let msg = parse_err_msg(
            "( runtime = rudzio::runtime::tokio::Multithread::new, suite = rudzio::common::context::Suite::<'_, Multithread>, test = rudzio::common::context::Test )",
        )?;
        anyhow::ensure!(
            msg.contains("bare") || msg.contains("generic") || msg.contains("lifetime"),
            "error should explain that generics are injected by the macro, got: {msg}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn rejects_turbofish_on_test(_ctx: &Test) -> anyhow::Result<()> {
        let msg = parse_err_msg(
            "( runtime = rudzio::runtime::tokio::Multithread::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test::<'_, Multithread> )",
        )?;
        anyhow::ensure!(
            msg.contains("bare") || msg.contains("generic") || msg.contains("lifetime"),
            "error should explain that generics are injected by the macro, got: {msg}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn runtime_type_strips_constructor_segment(_ctx: &Test) -> anyhow::Result<()> {
        let config: RuntimeConfig = syn::parse_str(
            "( runtime = my_crate::nested::Multithread::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test )",
        )?;
        anyhow::ensure!(
            render(&config.runtime_type()) == "my_crate::nested::Multithread",
            "runtime_type rendered as `{}`",
            render(&config.runtime_type()),
        );
        Ok(())
    }
}
