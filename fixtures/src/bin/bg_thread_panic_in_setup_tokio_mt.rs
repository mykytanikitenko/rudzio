//! Background-thread panic safety net.
//!
//! Mirrors a real-world failure mode (rustls / aws-smithy crypto
//! provider double-install) where `Suite::setup` spawns a `std::thread`
//! that panics, then the future returns `Ok` without observing the
//! panic. The user's test fn runs to completion; without the safety
//! net the binary would exit 0 and the panic message would be the only
//! sign anything went wrong.
//!
//! With the safety net (panic-hook counter + runner end-of-run check),
//! the binary exits 1 and prints a "rudzio: N background-thread
//! panic(s) detected" line on stderr.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::thread;
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::tokio_util::sync::CancellationToken;

struct BgPanicSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for BgPanicSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BgPanicSuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for BgPanicSuite<'suite_context, R>
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
        // Spawn a thread that panics. The setup future returns Ok
        // before the panic — the panic happens on a thread we don't
        // own, so our `catch_unwind` around setup never sees it.
        // Without the safety net, the test summary would say `0 failed`.
        let _join = thread::spawn(|| {
            panic!("simulated_bg_thread_panic_during_setup");
        });
        // Brief sleep so the spawned thread reliably panics before the
        // runner reaches its end-of-run bg-panic check.
        ::tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(Self {
            _marker: PhantomData,
        })
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
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
        suite = BgPanicSuite,
        test = TrivialTest,
    ),
])]
mod tests {
    use super::TrivialTest;

    #[rudzio::test]
    fn body_passes_despite_bg_panic(_ctx: &TrivialTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
