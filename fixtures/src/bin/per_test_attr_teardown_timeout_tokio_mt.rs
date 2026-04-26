//! Per-test attribute teardown timeout override.
//!
//! No CLI timeouts. The single test is annotated
//! `#[rudzio::test(teardown_timeout = 1)]`; its `Test::teardown` always
//! hangs. The macro-emitted override applies only to this test's
//! teardown phase. Body passes; teardown times out and contributes to
//! `teardown_failures`.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use tokio::time::sleep;

/// Suite context for the per-test teardown-timeout-attribute fixture.
struct AttrTeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

/// Per-test context whose `teardown` always hangs to exercise the
/// per-test `teardown_timeout` attribute override.
struct HangingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
}

impl<'suite_context, R> fmt::Debug for AttrTeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttrTeardownSuite").finish_non_exhaustive()
    }
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

impl<'suite_context, R> context::Suite<'suite_context, R> for AttrTeardownSuite<'suite_context, R>
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

    async fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(HangingTeardownTest {
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

impl<'test_context, R> context::Test<'test_context, R> for HangingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(self, cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        let _unused: Option<()> = cancel
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30)).await;
            })
            .await;
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = AttrTeardownSuite,
        test = HangingTeardownTest,
    ),
])]
mod tests {
    use super::HangingTeardownTest;

    #[rudzio::test(teardown_timeout = 1)]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture asserts the per-test teardown_timeout attribute fires while the body itself merely passes; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn attr_teardown_times_out(_ctx: &HangingTeardownTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
