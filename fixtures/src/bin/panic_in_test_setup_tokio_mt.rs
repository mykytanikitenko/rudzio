//! Exercises the `catch_unwind` wrapper around `Suite::context`
//! (per-test setup).
//!
//! Suite setup succeeds; per-test context creation panics rather than
//! returning Err. The wrapper turns the panic into a
//! `TestOutcome::SetupFailed` carrying the panic message. The test
//! shows up in output with the distinct `[SETUP]` status tag and the
//! run exits with code 1.

use std::convert::Infallible;
use std::fmt;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

/// Suite whose per-test [`context::Suite::context`] always panics.
struct PanickingContextSuite<'suite_context, R>
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

impl<'suite_context, R> fmt::Debug for PanickingContextSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PanickingContextSuite")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R>
    for PanickingContextSuite<'suite_context, R>
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
        clippy::panic,
        reason = "this fixture exercises the catch_unwind wrapper around Suite::context (per-test setup); the body must panic to verify the runner reports the failure as TestOutcome::SetupFailed with the panic message"
    )]
    async fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        panic!("test_setup_panicked_by_design")
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

/// Per-test context placeholder; never actually constructed because
/// [`PanickingContextSuite::context`] panics.
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

impl<'test_context, R> fmt::Debug for NeverBuiltTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeverBuiltTest").finish_non_exhaustive()
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
        suite = PanickingContextSuite,
        test = NeverBuiltTest,
    ),
])]
mod tests {
    use super::NeverBuiltTest;

    #[rudzio::test]
    #[expect(
        clippy::unreachable,
        reason = "this fixture exercises panic_in_test_setup; Suite::context panics before the body runs, so the body must be unreachable to confirm the runner never invoked it"
    )]
    fn body_never_runs(_ctx: &NeverBuiltTest) -> anyhow::Result<()> {
        unreachable!("body must not run when context() panicked")
    }
}

#[rudzio::main]
fn main() {}
