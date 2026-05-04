//! Per-test setup timeout fixture.
//!
//! `Suite::context` (per-test setup) sleeps past
//! `--test-setup-timeout`. The phase wrapper drops the in-flight context
//! future and the test reports `[SETUP]` with a "setup timed out"
//! message. Per-test teardown does NOT run (no context was ever
//! constructed) — this is the same invariant the panic-in-test-setup
//! fixture verifies.

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

/// Suite whose [`context::Suite::context`] hangs past the configured
/// per-test setup timeout, exercising the phase wrapper's timeout
/// branch.
struct HangingContextSuite<'suite_context, R>
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

/// Per-test context placeholder; never actually constructed because
/// [`HangingContextSuite::context`] is dropped on timeout.
struct NeverBuiltTest<'test_context, R>
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

impl<'suite_context, R> fmt::Debug for HangingContextSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HangingContextSuite")
            .finish_non_exhaustive()
    }
}

impl<'test_context, R> fmt::Debug for NeverBuiltTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeverBuiltTest").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for HangingContextSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = NeverBuiltTest<'test_context, R>
    where
        Self: 'test_context;

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts the test-setup-timeout phase wrapper drops the in-flight context() future before completion; the println! after the sleep is the unreached marker that the integration test greps for absence"
    )]
    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        // Hang past `--test-setup-timeout=1`. Cooperate with the per-test
        // token so we bail out the moment the wrapper signals timeout
        // (the same token gets cancelled by the wrapper).
        let _unused = cancel
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30_u64)).await;
            })
            .await;
        println!("hanging_test_setup_unreached_marker");
        Ok(NeverBuiltTest {
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

impl<'test_context, R> context::Test<'test_context, R> for NeverBuiltTest<'test_context, R>
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
        reason = "this fixture asserts Test::teardown does not run when context() timed out (no context was constructed); the println! is the unreached marker that the integration test greps for absence"
    )]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        println!("test_teardown_must_not_run_marker");
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = HangingContextSuite,
        test = NeverBuiltTest,
    ),
])]
mod tests {
    use super::NeverBuiltTest;

    #[rudzio::test]
    #[expect(
        clippy::unreachable,
        reason = "this fixture asserts the test body never runs when Suite::context() timed out; unreachable!() guards that contract — if it ever fires, the runner failed to honor the timeout"
    )]
    fn body_never_runs(_ctx: &NeverBuiltTest) -> anyhow::Result<()> {
        unreachable!("body must not run when context() timed out");
    }
}

#[rudzio::main]
fn main() {}
