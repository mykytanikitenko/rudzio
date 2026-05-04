//! Tests for the `#[rudzio::test(...)]` attribute argument parser.
//!
//! Covers the new phase-timeout overrides (`timeout`, `setup_timeout`,
//! `teardown_timeout`) alongside the existing `benchmark` arg. The
//! parser must surface unknown keys as a `syn::Error` pointing at the
//! offending token — silently dropping the override would leave a user
//! wondering why `--test-timeout=300` is being ignored on a single test.

use syn::{ItemFn, parse_quote};

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{compio, embassy};
use rudzio_macro_internals::codegen::extract_test_attr_args;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{ItemFn, Test, extract_test_attr_args, parse_quote};

    /// A14. `timeout = N` becomes `Some(N)` on the parsed args struct.
    /// The benchmark slot stays `None` when no benchmark is requested.
    #[rudzio::test]
    fn parse_test_attr_extracts_timeout_secs(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[rudzio::test(timeout = 5)]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let args = extract_test_attr_args(&func)?;
        anyhow::ensure!(
            args.timeout_secs == Some(5),
            "timeout_secs = {:?}",
            args.timeout_secs
        );
        anyhow::ensure!(args.setup_timeout_secs.is_none());
        anyhow::ensure!(args.teardown_timeout_secs.is_none());
        anyhow::ensure!(args.benchmark.is_none());
        Ok(())
    }

    /// A15. `setup_timeout = N` populates the dedicated setup slot
    /// without touching the body or teardown overrides.
    #[rudzio::test]
    fn parse_test_attr_extracts_setup_timeout_secs(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[rudzio::test(setup_timeout = 30)]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let args = extract_test_attr_args(&func)?;
        anyhow::ensure!(
            args.setup_timeout_secs == Some(30),
            "setup_timeout_secs = {:?}",
            args.setup_timeout_secs
        );
        anyhow::ensure!(args.timeout_secs.is_none());
        anyhow::ensure!(args.teardown_timeout_secs.is_none());
        Ok(())
    }

    /// A16. `teardown_timeout = N` populates the dedicated teardown
    /// slot.
    #[rudzio::test]
    fn parse_test_attr_extracts_teardown_timeout_secs(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[rudzio::test(teardown_timeout = 10)]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let args = extract_test_attr_args(&func)?;
        anyhow::ensure!(
            args.teardown_timeout_secs == Some(10),
            "teardown_timeout_secs = {:?}",
            args.teardown_timeout_secs
        );
        anyhow::ensure!(args.timeout_secs.is_none());
        anyhow::ensure!(args.setup_timeout_secs.is_none());
        Ok(())
    }

    /// A17. All three phase overrides in one attribute populate their
    /// respective slots independently. Order doesn't matter.
    #[rudzio::test]
    fn parse_test_attr_extracts_all_three_in_one_attr(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[rudzio::test(setup_timeout = 30, timeout = 5, teardown_timeout = 10)]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let args = extract_test_attr_args(&func)?;
        anyhow::ensure!(args.timeout_secs == Some(5));
        anyhow::ensure!(args.setup_timeout_secs == Some(30));
        anyhow::ensure!(args.teardown_timeout_secs == Some(10));
        Ok(())
    }

    /// A18. Phase overrides coexist with `benchmark = expr` — the new
    /// fields don't crowd out the existing slot. The benchmark expr is
    /// preserved verbatim.
    #[rudzio::test]
    fn parse_test_attr_combined_with_benchmark_works(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[rudzio::test(benchmark = ::rudzio::bench::strategy::Sequential(3), timeout = 7)]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let args = extract_test_attr_args(&func)?;
        anyhow::ensure!(args.timeout_secs == Some(7));
        anyhow::ensure!(args.benchmark.is_some(), "benchmark must be preserved");
        Ok(())
    }

    /// A19. An unrecognised key still returns a `syn::Error` whose
    /// message names every accepted key — so the diagnostic points the
    /// user at the right alternative instead of just "unknown".
    #[rudzio::test]
    fn parse_test_attr_unknown_key_errors_with_clear_message(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            #[rudzio::test(retry_count = 3)]
            async fn t() -> anyhow::Result<()> { Ok(()) }
        };
        let err = extract_test_attr_args(&func)
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected parse error for unknown key"))?;
        let msg = err.to_string();
        anyhow::ensure!(
            msg.contains("benchmark") && msg.contains("timeout"),
            "diagnostic must list accepted keys, got: {msg}"
        );
        Ok(())
    }
}
