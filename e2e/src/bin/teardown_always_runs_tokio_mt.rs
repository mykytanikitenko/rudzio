//! Teardown-always-runs fixture.
//!
//! The custom `TeardownTest` prints a marker in its `teardown` impl, and the
//! custom `TeardownGlobal` does the same. The test body itself times out, so
//! the assertions below prove that both per-test and global teardown still
//! run after the runner's per-test watchdog fires.

use std::time::Duration;

use rudzio::context::{Global, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = TeardownGlobal,
        test_context = TeardownTest,
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

pub struct TeardownGlobal<'cg, R>
where
    R: ::rudzio::runtime::Runtime<'cg> + Sync,
{
    _marker: std::marker::PhantomData<&'cg R>,
}

impl<'cg, R> std::fmt::Debug for TeardownGlobal<'cg, R>
where
    R: ::rudzio::runtime::Runtime<'cg> + Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownGlobal").finish_non_exhaustive()
    }
}

impl<'cg, R> Global<'cg, R> for TeardownGlobal<'cg, R>
where
    R: ::rudzio::runtime::Runtime<'cg> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test = TeardownTest<'cg, R>;

    async fn context(
        &self,
        cancel: CancellationToken,
    ) -> Result<Self::Test, Self::ContextError> {
        Ok(TeardownTest {
            cancel,
            _marker: std::marker::PhantomData,
        })
    }

    async fn setup(_rt: &'cg R, _cancel: CancellationToken) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: std::marker::PhantomData,
        })
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        println!("teardown_global_marker");
        Ok(())
    }
}

pub struct TeardownTest<'tc, R>
where
    R: ::rudzio::runtime::Runtime<'tc> + Sync,
{
    cancel: CancellationToken,
    _marker: std::marker::PhantomData<&'tc R>,
}

impl<'tc, R> std::fmt::Debug for TeardownTest<'tc, R>
where
    R: ::rudzio::runtime::Runtime<'tc> + Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeardownTest").finish_non_exhaustive()
    }
}

impl<'tc, R> Test<'tc, R> for TeardownTest<'tc, R>
where
    R: ::rudzio::runtime::Runtime<'tc> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        println!("teardown_test_marker");
        Ok(())
    }
}
