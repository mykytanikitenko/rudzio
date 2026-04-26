//! Per-test attribute setup timeout override.
//!
//! No CLI timeouts. The single test under this binary is annotated
//! `#[rudzio::test(setup_timeout = 1)]` and the suite's `context()`
//! always hangs. The macro-emitted override applies only to this test's
//! per-test-setup phase, so it bails out at 1s with a `[SETUP]` outcome.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use tokio::time::sleep;

/// Suite whose per-test [`context::Suite::context`] always hangs until
/// the cancel token fires (or 30s, whichever comes first).
struct HangingContextSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
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

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        let _unused = cancel
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30)).await;
            })
            .await;
        Ok(NeverBuiltTest {
            _marker: PhantomData,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
        })
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Per-test context placeholder; never actually constructed because
/// [`HangingContextSuite::context`] hangs past the per-test setup
/// timeout.
struct NeverBuiltTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
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

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
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

    #[rudzio::test(setup_timeout = 1)]
    #[expect(
        clippy::unreachable,
        reason = "this fixture exercises the per-test #[rudzio::test(setup_timeout=1)] override; Suite::context hangs and is killed by the per-test setup timeout, so the body must be unreachable"
    )]
    fn attr_setup_times_out(_ctx: &NeverBuiltTest) -> anyhow::Result<()> {
        unreachable!("body must not run when attribute setup timeout fires");
    }
}

#[rudzio::main]
fn main() {}
