//! Regression tests for the parallel-hardlimit deadlock that motivated
//! the swap from `Mutex+Condvar` to a runtime-agnostic async semaphore.
//!
//! Background: the previous primitive parked the calling OS thread on
//! `std::sync::Condvar` when the gate was full. Once a permit-holder
//! suspended (timer, IO, scheduler yield) while the only available
//! worker was parked at the Condvar trying to acquire the next permit,
//! the run wedged — the held permit could never be released and the
//! parked thread could never be unparked.
//!
//! These tests model that exact shape with `FuturesUnordered`: many
//! concurrent sub-futures contend for a hardlimit smaller than their
//! count, and each holder cooperatively suspends (`yield_now`) while
//! still holding its permit. With the async semaphore the gate yields
//! back to the executor and other sub-futures progress; if the gate
//! ever reverts to a Condvar primitive the test deadlocks and rudzio's
//! per-test timeout reports the regression. Driving the workload via
//! the shared rudzio runtime (no hand-built tokio runtime in a child
//! OS thread) keeps the regression assertion runtime-agnostic — the
//! same test runs on every adapter the suite is dispatched to.

use std::num::NonZeroUsize;
use std::sync::Arc;

use rudzio::common::context::{Suite, Test};
use rudzio::parallelism::HardLimit;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy};

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use rudzio::context::Test as _;
    use rudzio::futures_util::stream::{FuturesUnordered, StreamExt};

    use super::{Arc, HardLimit, NonZeroUsize, Test};

    /// Eight cooperative tasks contend for a 2-permit hardlimit. Each
    /// holder yields several times while holding its permit, exactly
    /// the shape that wedged the old `Mutex+Condvar` primitive: a
    /// holder suspended ⇒ another acquirer parked the worker on the
    /// Condvar ⇒ the held permit could never be released. The async
    /// semaphore yields cooperatively, so non-holders are polled
    /// while holders are suspended and the workload drains. Drives
    /// directly through `FuturesUnordered` in the test body so the
    /// deadlock reproduces on single-thread runtimes too — no hand-
    /// built tokio runtime in a child OS thread.
    #[rudzio::test]
    async fn acquire_yield_chain_does_not_deadlock(ctx: &Test) -> anyhow::Result<()> {
        let limit = Arc::new(HardLimit::with_sink(
            NonZeroUsize::new(2_usize),
            |_unused| {},
        ));
        let mut workers = FuturesUnordered::new();
        for _idx in 0_i32..8_i32 {
            let limit_clone = Arc::clone(&limit);
            workers.push(async move {
                let _guard = limit_clone.acquire().await;
                for _yield_idx in 0_i32..5_i32 {
                    ctx.yield_now().await;
                }
            });
        }
        while workers.next().await.is_some() {}
        Ok(())
    }

    /// One permit, two tasks, holder yields. Models the documented
    /// single-thread deadlock (`hardlimit < concurrency_limit`):
    /// the Condvar variant wedged the only worker so the holder's
    /// resume waker could never fire. The async semaphore yields,
    /// the holder resumes, the second task acquires.
    #[rudzio::test]
    async fn one_permit_two_yielding_tasks_does_not_deadlock(
        ctx: &Test,
    ) -> anyhow::Result<()> {
        let limit = Arc::new(HardLimit::with_sink(
            NonZeroUsize::new(1_usize),
            |_unused| {},
        ));
        let mut workers = FuturesUnordered::new();
        for _idx in 0_i32..2_i32 {
            let limit_clone = Arc::clone(&limit);
            workers.push(async move {
                let _guard = limit_clone.acquire().await;
                for _yield_idx in 0_i32..3_i32 {
                    ctx.yield_now().await;
                }
            });
        }
        while workers.next().await.is_some() {}
        Ok(())
    }

    /// Control: pure-CPU holders never hit the deadlock under either
    /// primitive — they don't yield, so the executor isn't asked to
    /// poll a non-holder while a holder is suspended. Confirms the
    /// regression gates above aren't producing false positives by
    /// virtue of the workload itself being trivially deadlock-free.
    #[rudzio::test]
    async fn pure_cpu_holders_finish_under_contention(_ctx: &Test) -> anyhow::Result<()> {
        let limit = Arc::new(HardLimit::with_sink(
            NonZeroUsize::new(2_usize),
            |_unused| {},
        ));
        let mut workers = FuturesUnordered::new();
        for _idx in 0_i32..8_i32 {
            let limit_clone = Arc::clone(&limit);
            workers.push(async move {
                let _guard = limit_clone.acquire().await;
                let mut sum = 0_u64;
                for value in 0_u64..10_000_u64 {
                    sum = sum.wrapping_add(value);
                }
                ::std::hint::black_box(sum);
            });
        }
        while workers.next().await.is_some() {}
        Ok(())
    }
}
