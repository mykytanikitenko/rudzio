//! Exercises the `catch_unwind` wrapper around `Suite::setup`.
//!
//! Setup panics rather than returning Err. Without the wrapper, the
//! panic would unwind through the runtime thread and the runner's
//! join handler would surface a generic "runtime thread panicked"
//! diagnostic with no link to the suite. The wrapper turns it into
//! a structured `[FAIL]   setup` lifecycle line carrying the panic
//! message, and every test in the group is reported as Cancelled
//! (it never had a chance to run).

use std::convert::Infallible;
use std::fmt;
use std::future::ready;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

/// Per-test context that is never built — `context()` is unreachable
/// because the suite's `setup` panics before any test gets a chance.
struct NeverBuilt<'test_context, R>
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

/// Suite whose [`context::Suite::setup`] always panics.
struct PanickingSetupSuite<'suite_context, R>
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

impl<'test_context, R> fmt::Debug for NeverBuilt<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeverBuilt").finish_non_exhaustive()
    }
}

impl<'suite_context, R> fmt::Debug for PanickingSetupSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PanickingSetupSuite")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for PanickingSetupSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = NeverBuilt<'test_context, R>
    where
        Self: 'test_context;

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    #[expect(
        clippy::unreachable,
        reason = "this fixture asserts Suite::setup's panic prevents context() from ever being invoked; the unreachable!() guards that contract — the panic must occur in setup() to exercise that path"
    )]
    fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context Config,
    ) -> impl Future<Output = Result<Self::Test<'test_context>, Self::ContextError>> + Send + 'test_context
    {
        Box::pin(async move { unreachable!("context() must not run when setup panicked") })
    }

    fn rt(&self) -> &'suite_context R {
        self.rt
    }

    #[expect(
        clippy::panic,
        reason = "this fixture asserts the runner's catch_unwind wrapper turns a Suite::setup panic into a structured [FAIL] setup line; the panic must occur to exercise that path"
    )]
    fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> impl Future<Output = Result<Self, Self::SetupError>> + Send + 'suite_context {
        Box::pin(async move { panic!("suite_setup_panicked_by_design") })
    }

    fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), Self::TeardownError>> + Send + 'suite_context {
        ready(Ok(()))
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

impl<'test_context, R> context::Test<'test_context, R> for NeverBuilt<'test_context, R>
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

    fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), Self::TeardownError>> + Send + 'test_context {
        ready(Ok(()))
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = PanickingSetupSuite,
        test = NeverBuilt,
    ),
])]
mod tests {
    use super::NeverBuilt;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture asserts the test never runs because Suite::setup panicked; the trivial body would only execute if the catch_unwind wrapper failed, and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn never_runs(_ctx: &NeverBuilt) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
