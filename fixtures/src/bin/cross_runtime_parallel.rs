//! Proves the per-runtime-group std threads run concurrently.
//!
//! Two runtime groups (tokio multi-thread + compio) each have one test.
//! Both tests block on a shared `std::sync::Barrier::new(2)` via the
//! runtime's `spawn_blocking`. If the two groups run in parallel (one
//! OS thread per group) the barrier releases and both pass. If they
//! were serialised, the first group's test would wait forever — the
//! watchdog thread then exits the process with code 2.

use std::error::Error;
use std::fmt;
use std::process::exit;
use std::sync::{Arc, Barrier, BarrierWaitResult, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::compio::Runtime as CompioRuntime;
use rudzio::runtime::tokio::Multithread;
use rudzio::runtime::{JoinError, Runtime};
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

/// Maximum time the watchdog thread waits before aborting the process.
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared suite context starting the watchdog once per process.
struct CrossSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Per-suite cancellation token.
    cancel: CancellationToken,
    /// Borrow of the async runtime driving the suite context.
    rt: &'suite_context R,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
}

/// Per-test context exposing `spawn_blocking` on the group's runtime.
struct CrossTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Per-test cancellation token.
    cancel: CancellationToken,
    /// Resolved CLI/env configuration.
    config: &'test_context Config,
    /// Borrow of the async runtime driving this test.
    rt: &'test_context R,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
}

/// Sentinel error type that never occurs in practice.
#[derive(Debug)]
struct NeverFails;

impl fmt::Display for NeverFails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NeverFails")
    }
}

impl Error for NeverFails {}

impl<'suite_context, R> fmt::Debug for CrossSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrossSuite").finish_non_exhaustive()
    }
}

impl<'test_context, R> CrossTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Hand off a blocking closure to the group's async runtime.
    fn spawn_blocking<F, T>(
        &self,
        func: F,
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'test_context
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.rt.spawn_blocking(func)
    }
}

impl<'test_context, R> fmt::Debug for CrossTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrossTest").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for CrossSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = NeverFails;
    type SetupError = NeverFails;
    type TeardownError = NeverFails;
    type Test<'test_context>
        = CrossTest<'test_context, R>
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
        Ok(CrossTest {
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
        start_watchdog();
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

impl<'test_context, R> context::Test<'test_context, R> for CrossTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = NeverFails;

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

/// Lazily construct the `Barrier` shared across the two runtime groups.
fn shared_barrier() -> Arc<Barrier> {
    static BARRIER: OnceLock<Arc<Barrier>> = OnceLock::new();
    Arc::clone(BARRIER.get_or_init(|| Arc::new(Barrier::new(2))))
}

/// Spawn a one-shot watchdog thread that aborts the process if the
/// cross-runtime barrier hasn't released within [`WATCHDOG_TIMEOUT`].
#[expect(
    clippy::print_stderr,
    clippy::exit,
    reason = "this fixture's watchdog must abort the process with a non-zero code if the cross-runtime barrier never releases (the only way to surface a stuck-serialised run from a thread that has no async runtime), and eprintln! is the deliberate channel for the diagnostic line the integration test greps"
)]
fn start_watchdog() {
    static WATCHDOG: OnceLock<()> = OnceLock::new();
    let _init: &() = WATCHDOG.get_or_init(|| {
        let _jh: JoinHandle<()> = thread::spawn(|| {
            thread::sleep(WATCHDOG_TIMEOUT);
            eprintln!(
                "cross-runtime watchdog: > {}s elapsed without barrier release — exiting",
                WATCHDOG_TIMEOUT.as_secs(),
            );
            exit(2);
        });
    });
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = CrossSuite,
        test = CrossTest,
    ),
    (
        runtime = CompioRuntime::new,
        suite = CrossSuite,
        test = CrossTest,
    ),
])]
mod tests {
    use super::{BarrierWaitResult, CrossTest, shared_barrier};

    #[rudzio::test]
    async fn waits_on_cross_runtime_barrier(ctx: &CrossTest) -> anyhow::Result<()> {
        let barrier = shared_barrier();
        let _wait: BarrierWaitResult = ctx
            .spawn_blocking(move || barrier.wait())
            .await
            .map_err(|err| anyhow::anyhow!("spawn_blocking failed: {err}"))?;
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
