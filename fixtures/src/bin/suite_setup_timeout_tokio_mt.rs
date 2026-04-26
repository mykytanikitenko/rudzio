//! Suite-setup timeout fixture.
//!
//! `Suite::setup` sleeps past the per-suite-setup budget. The phase
//! wrapper drops the in-flight setup future, every queued test reports
//! `[CANCEL]`, the lifecycle line for the suite shows `[TIMEOUT]`, and
//! the binary exits non-zero. Companion to `setup_failure_tokio_mt`
//! (which exercises the Err-return path) and
//! `panic_in_suite_setup_tokio_mt` (the panic path).

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use tokio::time::sleep;

/// Suite whose [`context::Suite::setup`] hangs past the configured
/// per-suite-setup timeout, exercising the phase wrapper's timeout
/// branch.
struct HangingSetupSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

/// Per-test context placeholder; never actually constructed because
/// [`HangingSetupSuite::setup`] is dropped on timeout.
struct NeverBuiltTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
}

impl<'suite_context, R> fmt::Debug for HangingSetupSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HangingSetupSuite").finish_non_exhaustive()
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

impl<'suite_context, R> context::Suite<'suite_context, R> for HangingSetupSuite<'suite_context, R>
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
        _cancel: CancellationToken,
        _config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(NeverBuiltTest {
            _marker: PhantomData,
        })
    }

    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts the suite-setup-timeout phase wrapper drops the in-flight setup future before completion; the println! after the sleep is the unreached marker that the integration test greps for absence"
    )]
    async fn setup(
        _rt: &'suite_context R,
        cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        // Hang well past the integration test's `--suite-setup-timeout=1`.
        // Cooperate with the cancel token so we exit the moment the
        // wrapper signals timeout (otherwise the test runner has to wait
        // 30s for the future to be dropped).
        let _unused = cancel
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30_u64)).await;
            })
            .await;
        println!("hanging_suite_setup_unreached_marker");
        Ok(Self {
            _marker: PhantomData,
        })
    }

    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts Suite::teardown does not run when setup timed out; the println! is the unreached marker that the integration test greps for absence"
    )]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        println!("teardown_must_not_run_marker");
        Ok(())
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

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture asserts queued tests are Cancelled when Suite::setup times out; the never_runs() body trivially returns Ok(()) so its anyhow::Result<()> wrapper is redundant, but the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = HangingSetupSuite,
        test = NeverBuiltTest,
    ),
])]
mod tests {
    use super::NeverBuiltTest;

    #[rudzio::test]
    fn never_runs(_ctx: &NeverBuiltTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
