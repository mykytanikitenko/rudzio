//! Per-test teardown timeout fixture.
//!
//! Test body passes; `Test::teardown` sleeps past
//! `--test-teardown-timeout`. The phase wrapper drops the teardown
//! future, the test reports a `[TIMEOUT] teardown` lifecycle line and
//! contributes to `teardown_failures` (so the run exits non-zero), but
//! the test body's outcome is still `Passed`.

use std::convert::Infallible;
use std::fmt;
use std::time::Duration;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;
use tokio::time::sleep;

/// Per-test context whose [`context::Test::teardown`] hangs past the
/// configured per-test teardown timeout, exercising the phase
/// wrapper's timeout branch.
struct HangingTeardownTest<'test_context, R>
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

/// Suite that constructs [`HangingTeardownTest`] per test; suite
/// setup/teardown themselves are no-ops.
struct HangingTeardownTestSuite<'suite_context, R>
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

impl<'test_context, R> fmt::Debug for HangingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HangingTeardownTest")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> fmt::Debug for HangingTeardownTestSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HangingTeardownTestSuite")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R>
    for HangingTeardownTestSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = HangingTeardownTest<'test_context, R>
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
        Ok(HangingTeardownTest {
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
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

impl<'test_context, R> context::Test<'test_context, R> for HangingTeardownTest<'test_context, R>
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

    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts the test-teardown-timeout phase wrapper drops the in-flight teardown future before completion; the println! after the sleep is the unreached marker that the integration test greps for absence"
    )]
    async fn teardown(self, cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        let _unused = cancel
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30_u64)).await;
            })
            .await;
        println!("hanging_test_teardown_unreached_marker");
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture asserts the test body's Passed outcome survives a teardown timeout; the body trivially returns Ok(()) so its anyhow::Result<()> wrapper is redundant, but the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = HangingTeardownTestSuite,
        test = HangingTeardownTest,
    ),
])]
mod tests {
    use super::HangingTeardownTest;

    #[rudzio::test]
    fn body_passes_then_teardown_times_out(_ctx: &HangingTeardownTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
