//! Exercises the path where `Suite::teardown` returns `Err`.
//!
//! Setup succeeds, the test body runs, then teardown errors. The runner
//! must emit a `[FAIL]   teardown <suite>` lifecycle line carrying the
//! error's `Display`. The test itself still counts as passed (teardown
//! failures are non-fatal for the per-test outcome) but the overall run
//! is recorded as failed via the failures section.

use std::error::Error;
use std::fmt;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

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
    /// Per-suite cancellation token.
    cancel: CancellationToken,
    /// Borrow of the async runtime driving this suite.
    rt: &'suite_context R,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
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
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = TeardownErr;
    type SetupError = TeardownErr;
    type TeardownError = TeardownErr;
    type Test<'test_context>
        = TrivialTest<'test_context, R>
    where
        Self: 'test_context;

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(TrivialTest {
            cancel,
            config,
            rt: self.rt,
            tracker: self.tracker.clone(),
        })
    }

    fn rt(&self) -> &'suite_context R {
        self.rt
    }

    async fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            cancel: cancel.child_token(),
            rt,
            tracker: TaskTracker::new(),
        })
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Err(TeardownErr)
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

/// Per-test context that does nothing. Teardown is the per-test
/// teardown, not the suite teardown — leave it Ok so the failure point
/// is unambiguous.
struct TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Per-test cancellation token.
    cancel: CancellationToken,
    /// Resolved CLI/env configuration.
    config: &'test_context Config,
    /// Borrow of the async runtime driving this test.
    rt: &'test_context R,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
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

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn config(&self) -> &Config {
        self.config
    }

    fn rt(&self) -> &'test_context R {
        self.rt
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = FailingTeardownSuite,
        test = TrivialTest,
    ),
])]
mod tests {
    use super::TrivialTest;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture's body must succeed (Ok(())) so suite teardown failure is the unambiguous failure point; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn body_runs_then_teardown_fails(_ctx: &TrivialTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
