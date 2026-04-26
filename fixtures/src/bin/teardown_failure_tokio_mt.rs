//! Exercises the path where `Suite::teardown` returns `Err`.
//!
//! Setup succeeds, the test body runs, then teardown errors. The runner
//! must emit a `[FAIL]   teardown <suite>` lifecycle line carrying the
//! error's `Display`. The test itself still counts as passed (teardown
//! failures are non-fatal for the per-test outcome) but the overall run
//! is recorded as failed via the failures section.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;

/// Error type used to fail teardown on purpose.
#[derive(Debug)]
struct TeardownErr;

impl fmt::Display for TeardownErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("teardown_failed_by_design")
    }
}

impl Error for TeardownErr {}

/// Suite whose [`context::Suite::teardown`] always errors. Setup and
/// per-test context creation both succeed.
struct FailingTeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for FailingTeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailingTeardownSuite")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R>
    for FailingTeardownSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = TeardownErr;
    type SetupError = TeardownErr;
    type TeardownError = TeardownErr;
    type Test<'test_context>
        = TrivialTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(TrivialTest {
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

    async fn teardown(self, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<(), Self::TeardownError> {
        Err(TeardownErr)
    }
}

/// Per-test context that does nothing. Teardown is the per-test
/// teardown, not the suite teardown — leave it Ok so the failure point
/// is unambiguous.
struct TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrivialTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = TeardownErr;

    async fn teardown(self, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = FailingTeardownSuite,
        test = TrivialTest,
    ),
])]
mod tests {
    use super::TrivialTest;

    #[rudzio::test]
    fn body_runs_then_teardown_fails(_ctx: &TrivialTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
