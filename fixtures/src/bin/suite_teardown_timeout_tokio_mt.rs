//! Suite-teardown timeout fixture.
//!
//! Setup and the test body succeed; suite teardown sleeps past
//! `--suite-teardown-timeout`. The phase wrapper drops the teardown
//! future, the lifecycle line shows `[TIMEOUT]`, and the binary exits
//! non-zero (teardown failures are non-fatal for the per-test outcome
//! but the run is failed via `teardown_failures`).

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::tokio_util::sync::CancellationToken;

struct HangingTeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for HangingTeardownSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HangingTeardownSuite")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R>
    for HangingTeardownSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = TrivialTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(TrivialTest {
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

    async fn teardown(self, cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        let _unused = cancel
            .run_until_cancelled(async {
                ::tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("hanging_suite_teardown_unreached_marker");
        Ok(())
    }
}

struct TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrivialTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for TrivialTest<'test_context, R>
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
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = HangingTeardownSuite,
        test = TrivialTest,
    ),
])]
mod tests {
    use super::TrivialTest;

    #[rudzio::test]
    fn body_passes_then_suite_teardown_times_out(_ctx: &TrivialTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
