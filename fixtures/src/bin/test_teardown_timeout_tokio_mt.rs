//! Per-test teardown timeout fixture.
//!
//! Test body passes; `Test::teardown` sleeps past
//! `--test-teardown-timeout`. The phase wrapper drops the teardown
//! future, the test reports a `[TIMEOUT] teardown` lifecycle line and
//! contributes to `teardown_failures` (so the run exits non-zero), but
//! the test body's outcome is still `Passed`.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::tokio_util::sync::CancellationToken;

struct HangingTeardownTestSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    _marker: PhantomData<&'suite_context R>,
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
    R: for<'r> Runtime<'r> + Sync,
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

struct HangingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    _marker: PhantomData<&'test_context R>,
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

impl<'test_context, R> context::Test<'test_context, R> for HangingTeardownTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(self, cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        let _unused = cancel
            .run_until_cancelled(async {
                ::tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("hanging_test_teardown_unreached_marker");
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
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
