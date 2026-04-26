//! Teardown-always-runs fixture.
//!
//! The custom `TeardownTest` prints a marker in its `teardown` impl, and the
//! custom `TeardownSuite` does the same. The test body itself times out, so
//! the assertions below prove that both per-test and suite teardown still
//! run after the runner's per-test watchdog fires.

use std::time::Duration;

use rudzio::context::{Suite, Test};

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = TeardownSuite,
        test = TeardownTest,
    ),
])]
mod tests {
    use super::{Duration, TeardownTest};

    #[rudzio::test]
    async fn body_times_out(ctx: &TeardownTest) -> anyhow::Result<()> {
        // Cooperates with the cancellation token: when the runner's per-test
        // watchdog fires it cancels `ctx.cancel`, and the test returns.
        let _unused = ctx
            .cancel
            .run_until_cancelled(async {
                ::tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("body_times_out_unreached_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}

use std::convert::Infallible;

use ::rudzio::tokio_util::sync::CancellationToken;

pub struct TeardownSuite<'suite_context, R>
where
    R: ::rudzio::runtime::Runtime<'suite_context> + Sync,
{
    _marker: std::marker::PhantomData<&'suite_context R>,
}

impl<'suite_context, R> std::fmt::Debug for TeardownSuite<'suite_context, R>
where
    R: ::rudzio::runtime::Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownSuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> Suite<'suite_context, R> for TeardownSuite<'suite_context, R>
where
    R: for<'r> ::rudzio::runtime::Runtime<'r> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = TeardownTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(TeardownTest {
            cancel,
            _marker: std::marker::PhantomData,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: std::marker::PhantomData,
        })
    }

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        println!("teardown_suite_marker");
        Ok(())
    }
}

pub struct TeardownTest<'test_context, R>
where
    R: ::rudzio::runtime::Runtime<'test_context> + Sync,
{
    cancel: CancellationToken,
    _marker: std::marker::PhantomData<&'test_context R>,
}

impl<'test_context, R> std::fmt::Debug for TeardownTest<'test_context, R>
where
    R: ::rudzio::runtime::Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> Test<'test_context, R> for TeardownTest<'test_context, R>
where
    R: ::rudzio::runtime::Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        println!("teardown_test_marker");
        Ok(())
    }
}
