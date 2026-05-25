//! Compile-level assertions for the rudzio macro, driven by `trybuild`.

use rudzio::common::context::Suite;
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};

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
    use rudzio::common::context::Test;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn sync_test_should_compile(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/sync_test.rs");
        Ok(())
    }

    /// **Bug #15** regression: a `#[rudzio::suite]`-annotated mod with
    /// an inner attribute (`#![expect(...)]`) inside the mod body
    /// must compile cleanly. Pre-fix the macro splatted the inner
    /// attr to the call site, producing
    /// `error: an inner attribute is not permitted in this context`.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_in_suite_mod_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_in_suite_mod.rs");
        Ok(())
    }

    /// Bug #15 defensive: `#![allow(...)]` inner attribute kind.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_allow_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_allow.rs");
        Ok(())
    }

    /// Bug #15 defensive: `#![warn(...)]` inner attribute kind.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_warn_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_warn.rs");
        Ok(())
    }

    /// Bug #15 defensive: `#![deny(...)]` inner attribute kind. The
    /// fixture body does NOT trigger the chosen lint, so the test
    /// passes iff the attribute survives macro expansion.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_deny_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_deny.rs");
        Ok(())
    }

    /// Bug #15 defensive: `#![cfg_attr(...)]` inner attribute with
    /// nested meta tree.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_cfg_attr_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_cfg_attr.rs");
        Ok(())
    }

    /// Bug #15 defensive: tool-path inner attribute
    /// (`#![rustfmt::skip]` and friends) with multi-segment path.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_tool_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_tool.rs");
        Ok(())
    }

    /// Bug #15 defensive: inner doc comment (`//! ...`) desugars to
    /// `#![doc = "..."]` and must survive the partition.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_doc_comment_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_doc_comment.rs");
        Ok(())
    }

    /// Bug #15 defensive: multiple consecutive inner attributes must
    /// ALL survive, in order.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_multiple_consecutive_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_multiple_consecutive.rs");
        Ok(())
    }

    /// Bug #15 defensive: outer attribute on the mod plus inner
    /// attribute inside — partition must keep outer outside and inner
    /// inside.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_mixed_outer_and_inner_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_mixed_outer_and_inner.rs");
        Ok(())
    }

    /// Bug #15 defensive: inner attribute survives even when the
    /// suite list contains `#[cfg(...)]`-gated runtime configs (a
    /// distinct codegen branch).
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_with_cfg_gated_runtime_configs_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_with_cfg_gated_runtime_configs.rs");
        Ok(())
    }

    /// Bug #15 defensive: mod-level inner attribute plus outer
    /// attributes on items INSIDE the mod must coexist — partition
    /// must distinguish the two attachment points.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_with_outer_attrs_on_items_compiles(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_with_outer_attrs_on_items.rs");
        Ok(())
    }

    /// Bug #15 BEHAVIORAL: inner `#![allow(unused_variables)]` must
    /// actually SUPPRESS the lint inside the mod body. File-level
    /// `#![deny(unused_variables)]` would otherwise fire on the
    /// deliberately-unused `let unused`. Compile-pass proves the
    /// inner attribute is bound at a scope that covers the test fn
    /// body.
    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn inner_attr_behavioral_allow_suppresses_outer_deny_compiles(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/inner_attr_behavioral_allow_suppresses_outer_deny.rs");
        Ok(())
    }
}
