//! Teardown-always-runs fixture.
//!
//! The custom `TeardownTest` prints a marker in its `teardown` impl, and the
//! custom `TeardownSuite` does the same. The test body itself times out, so
//! the assertions below prove that both per-test and suite teardown still
//! run after the runner's per-test watchdog fires.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use rudzio::context::{Suite, Test};
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use tokio::time::sleep;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = TeardownSuite,
        test = TeardownTest,
    ),
])]
mod tests {
    use super::{Duration, TeardownTest, sleep};

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture verifies the per-test watchdog fires before the body's 30 s sleep completes; the marker after the cancelled sleep must never appear, and integration tests grep stdout to confirm absence"
    )]
    async fn body_times_out(ctx: &TeardownTest) -> anyhow::Result<()> {
        // Cooperates with the cancellation token: when the runner's per-test
        // watchdog fires it cancels `ctx.cancel`, and the test returns.
        let _unused = ctx
            .cancel
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("body_times_out_unreached_marker");
        Ok(())
    }
}

/// Suite that prints a marker from its teardown so integration tests can
/// confirm suite-level teardown ran even after a per-test body timeout.
pub struct TeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

/// Per-test context that holds a cancel token (so the test body can cooperate
/// with the runner's per-test watchdog) and prints a marker from its
/// teardown impl.
pub struct TeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
    /// Cancel token plumbed in from the suite's `context` impl; the test
    /// body races a 30 s sleep against this token so it returns when the
    /// per-test watchdog fires.
    cancel: CancellationToken,
}

#[rudzio::main]
fn main() {}

impl<'suite_context, R> fmt::Debug for TeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeardownSuite").finish_non_exhaustive()
    }
}

impl<'test_context, R> fmt::Debug for TeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeardownTest").finish_non_exhaustive()
    }
}

impl<'suite_context, R> Suite<'suite_context, R> for TeardownSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = TeardownTest<'test_context, R>
    where
        Self: 'test_context;

    #[inline]
    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(TeardownTest {
            cancel,
            _marker: PhantomData,
        })
    }

    #[inline]
    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
        })
    }

    #[inline]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture verifies that suite-level teardown runs after a per-test body timeout; the marker is printed for integration tests to grep"
    )]
    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        println!("teardown_suite_marker");
        Ok(())
    }
}

impl<'test_context, R> Test<'test_context, R> for TeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    #[inline]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture verifies that per-test teardown runs after the runner's per-test watchdog fires; the marker is printed for integration tests to grep"
    )]
    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        println!("teardown_test_marker");
        Ok(())
    }
}
