//! Proves the runner dispatches tests concurrently.
//!
//! Three tests synchronise on a shared `tokio::sync::Barrier::new(3)` under
//! a 2s `tokio::time::timeout`. If the runner serialises, the first test
//! hits the barrier and times out before the others arrive — every test
//! fails. If the runner dispatches all three to the runtime together, the
//! barrier releases and all pass.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use tokio::sync::{Barrier, BarrierWaitResult};
use tokio::time::timeout;

/// Number of tests that must hit the barrier before anyone proceeds.
const PARTIES: usize = 3;
/// Budget each test has to reach the barrier before failing the fixture.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(2);

/// Sentinel error type that never occurs in practice.
#[derive(Debug)]
struct NeverFails;

impl fmt::Display for NeverFails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NeverFails")
    }
}

impl Error for NeverFails {}

/// Global context that owns the shared barrier.
struct ParallelGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'cg R>,
    /// Barrier shared across every per-test context in this group.
    barrier: Arc<Barrier>,
}

impl<'cg, R> fmt::Debug for ParallelGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParallelGlobal").finish_non_exhaustive()
    }
}

impl<'cg, R> context::Global<'cg, R> for ParallelGlobal<'cg, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = NeverFails;
    type SetupError = NeverFails;
    type TeardownError = NeverFails;
    type Test<'test_context>
        = ParallelTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(ParallelTest {
            _marker: PhantomData,
            barrier: Arc::clone(&self.barrier),
        })
    }

    async fn setup(_rt: &'cg R, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
            barrier: Arc::new(Barrier::new(PARTIES)),
        })
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Per-test context handing out a clone of the shared barrier.
struct ParallelTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'tc R>,
    /// Shared barrier from the global context.
    barrier: Arc<Barrier>,
}

impl<'tc, R> fmt::Debug for ParallelTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParallelTest").finish_non_exhaustive()
    }
}

impl<'tc, R> ParallelTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    /// Hand out a clone of the shared barrier.
    fn barrier(&self) -> Arc<Barrier> {
        Arc::clone(&self.barrier)
    }
}

impl<'tc, R> context::Test<'tc, R> for ParallelTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    type TeardownError = NeverFails;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = ParallelGlobal,
        test_context = ParallelTest,
    ),
])]
mod tests {
    use super::{BARRIER_TIMEOUT, BarrierWaitResult, ParallelTest, timeout};

    #[rudzio::test]
    async fn first_hits_barrier(ctx: &ParallelTest) -> anyhow::Result<()> {
        let barrier = ctx.barrier();
        let _wait: BarrierWaitResult = timeout(BARRIER_TIMEOUT, barrier.wait())
            .await
            .map_err(|_elapsed| {
                anyhow::anyhow!(
                    "barrier timed out \u{2014} runner did not dispatch concurrently"
                )
            })?;
        Ok(())
    }

    #[rudzio::test]
    async fn second_hits_barrier(ctx: &ParallelTest) -> anyhow::Result<()> {
        let barrier = ctx.barrier();
        let _wait: BarrierWaitResult = timeout(BARRIER_TIMEOUT, barrier.wait())
            .await
            .map_err(|_elapsed| {
                anyhow::anyhow!(
                    "barrier timed out \u{2014} runner did not dispatch concurrently"
                )
            })?;
        Ok(())
    }

    #[rudzio::test]
    async fn third_hits_barrier(ctx: &ParallelTest) -> anyhow::Result<()> {
        let barrier = ctx.barrier();
        let _wait: BarrierWaitResult = timeout(BARRIER_TIMEOUT, barrier.wait())
            .await
            .map_err(|_elapsed| {
                anyhow::anyhow!(
                    "barrier timed out \u{2014} runner did not dispatch concurrently"
                )
            })?;
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
