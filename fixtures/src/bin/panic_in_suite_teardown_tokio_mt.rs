//! Exercises the `catch_unwind` wrapper around `Suite::teardown`.
//!
//! Setup and the test body both succeed; teardown panics. The wrapper
//! turns the panic into a structured `[PANIC] teardown` lifecycle line
//! carrying the panic message, bumps `teardown_failures`, and the run
//! exits with code 1.

use std::convert::Infallible;
use std::fmt;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

/// Suite whose [`context::Suite::teardown`] always panics, exercising
/// the runner's `catch_unwind` wrapper around suite teardown.
struct PanickingTeardownSuite<'suite_context, R>
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

/// Per-test context with no state; the test body simply returns
/// `Ok(())` so we exercise teardown's panic handling rather than the
/// body's.
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

impl<'suite_context, R> fmt::Debug for PanickingTeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PanickingTeardownSuite")
            .finish_non_exhaustive()
    }
}

impl<'test_context, R> fmt::Debug for TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrivialTest").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R>
    for PanickingTeardownSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
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

    #[expect(
        clippy::panic,
        reason = "this fixture asserts the runner's catch_unwind wrapper turns a Suite::teardown panic into a structured [PANIC] teardown line; the panic must occur to exercise that path"
    )]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        panic!("suite_teardown_panicked_by_design")
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

impl<'test_context, R> context::Test<'test_context, R> for TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

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

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture asserts Suite::teardown's panic is captured by catch_unwind; the test body trivially returns Ok(()) so its anyhow::Result<()> wrapper is redundant, but the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = PanickingTeardownSuite,
        test = TrivialTest,
    ),
])]
mod tests {
    use super::TrivialTest;

    #[rudzio::test]
    fn body_runs_then_teardown_panics(_ctx: &TrivialTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
