//! Exercises the `catch_unwind` wrapper around `Test::teardown`
//! (per-test teardown).
//!
//! Suite setup, per-test context, and the test body all succeed.
//! Per-test teardown panics. The wrapper routes the panic through
//! `report_test_teardown_failure`, which renders a `[PANIC] teardown
//! <test>` line carrying the panic message and bumps the per-test
//! teardown counter. The run exits with code 1.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;

struct PassingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for PassingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PassingSuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for PassingSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = PanickingTeardownTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(PanickingTeardownTest {
            _marker: PhantomData,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
        })
    }

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Per-test context whose teardown always panics.
struct PanickingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for PanickingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PanickingTeardownTest")
            .finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for PanickingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        panic!("test_teardown_panicked_by_design")
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = PassingSuite,
        test = PanickingTeardownTest,
    ),
])]
mod tests {
    use super::PanickingTeardownTest;

    #[rudzio::test]
    fn body_runs_then_teardown_panics(_ctx: &PanickingTeardownTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
