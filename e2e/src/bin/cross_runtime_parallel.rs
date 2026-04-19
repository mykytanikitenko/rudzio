//! Proves the per-runtime-group std threads run concurrently.
//!
//! Two runtime groups (tokio multi-thread + compio) each have one test.
//! Both tests block on a shared `std::sync::Barrier::new(2)` via the
//! runtime's `spawn_blocking`. If the two groups run in parallel (one
//! OS thread per group) the barrier releases and both pass. If they
//! were serialised, the first group's test would wait forever — the
//! watchdog thread then exits the process with code 2.

// The watchdog thread intentionally short-circuits the process with
// `eprintln!` + `process::exit` when the barrier doesn't release.
#![allow(
    clippy::print_stderr,
    clippy::exit,
    reason = "watchdog uses stderr + exit(2) to short-circuit a deadlock"
)]

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::process::exit;
use std::sync::{Arc, Barrier, BarrierWaitResult, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::compio::Runtime as CompioRuntime;
use rudzio::runtime::tokio::Multithread;
use rudzio::runtime::{JoinError, Runtime};

/// Maximum time the watchdog thread waits before aborting the process.
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared global context starting the watchdog once per process.
struct CrossGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    /// Borrow of the async runtime driving the global context.
    rt: &'cg R,
}

/// Per-test context exposing `spawn_blocking` on the group's runtime.
struct CrossTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'tc R>,
    /// Borrow of the async runtime driving this test.
    rt: &'tc R,
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

impl<'cg, R> fmt::Debug for CrossGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrossGlobal").finish_non_exhaustive()
    }
}

impl<'tc, R> CrossTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    /// Hand off a blocking closure to the group's async runtime.
    fn spawn_blocking<F, T>(
        &self,
        func: F,
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'tc
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.rt.spawn_blocking(func)
    }
}

impl<'tc, R> fmt::Debug for CrossTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrossTest").finish_non_exhaustive()
    }
}

impl<'cg, R> context::Global<'cg, R> for CrossGlobal<'cg, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = NeverFails;
    type SetupError = NeverFails;
    type TeardownError = NeverFails;
    type Test<'test_context>
        = CrossTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(CrossTest {
            _marker: PhantomData,
            rt: self.rt,
        })
    }

    async fn setup(rt: &'cg R, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<Self, Self::SetupError> {
        start_watchdog();
        Ok(Self { rt })
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

impl<'tc, R> context::Test<'tc, R> for CrossTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    type TeardownError = NeverFails;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Lazily construct the `Barrier` shared across the two runtime groups.
fn shared_barrier() -> Arc<Barrier> {
    static BARRIER: OnceLock<Arc<Barrier>> = OnceLock::new();
    Arc::clone(BARRIER.get_or_init(|| Arc::new(Barrier::new(2))))
}

/// Spawn a one-shot watchdog thread that aborts the process if the
/// cross-runtime barrier hasn't released within [`WATCHDOG_TIMEOUT`].
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
        global_context = CrossGlobal,
        test_context = CrossTest,
    ),
    (
        runtime = CompioRuntime::new,
        global_context = CrossGlobal,
        test_context = CrossTest,
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
