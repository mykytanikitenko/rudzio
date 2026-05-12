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
use std::thread;
use std::time::Duration;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;
use tokio::time::sleep;

/// Suite context exercising the background-thread panic safety net.
struct BgPanicSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Per-suite cancellation token; child of the runner's root token.
    cancel: CancellationToken,
    /// Borrow of the async runtime driving this suite.
    rt: &'suite_context R,
    /// Suite-shared task tracker; drained by `Suite::teardown`.
    tracker: TaskTracker,
}

/// Per-test context with no state; the bg-panic scenario fires entirely
/// from `Suite::setup`.
struct TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Per-test cancellation token; child of the suite token.
    cancel: CancellationToken,
    /// Resolved CLI/env configuration, handed down from the suite.
    config: &'test_context Config,
    /// Borrow of the async runtime driving this test.
    rt: &'test_context R,
    /// Suite-shared task tracker (cloned from the suite).
    tracker: TaskTracker,
}

impl<'suite_context, R> fmt::Debug for BgPanicSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BgPanicSuite").finish_non_exhaustive()
    }
}

impl<'test_context, R> fmt::Debug for TrivialTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrivialTest").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for BgPanicSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = TrivialTest<'test_context, R>
    where
        Self: 'test_context;

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(TrivialTest {
            cancel,
            config,
            rt: self.rt,
            tracker: self.tracker.clone(),
        })
    }

    fn rt(&self) -> &'suite_context R {
        self.rt
    }

    async fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        // Spawn a thread that panics. The setup future returns Ok
        // before the panic — the panic happens on a thread we don't
        // own, so our `catch_unwind` around setup never sees it.
        // Without the safety net, the test summary would say `0 failed`.
        #[expect(
            clippy::panic,
            reason = "this fixture asserts the runner's background-thread panic safety net detects panics escaping a non-runtime std::thread; the panic must occur off-runtime to exercise that path"
        )]
        let _join: thread::JoinHandle<()> = thread::spawn(|| {
            panic!("simulated_bg_thread_panic_during_setup");
        });
        // Brief sleep so the spawned thread reliably panics before the
        // runner reaches its end-of-run bg-panic check.
        sleep(Duration::from_millis(100)).await;
        Ok(Self {
            cancel: cancel.child_token(),
            rt,
            tracker: TaskTracker::new(),
        })
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

impl<'test_context, R> context::Test<'test_context, R> for TrivialTest<'test_context, R>
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

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = BgPanicSuite,
        test = TrivialTest,
    ),
])]
mod tests {
    use super::TrivialTest;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture asserts the test body completes successfully despite a background-thread panic in Suite::setup; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn body_passes_despite_bg_panic(_ctx: &TrivialTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
