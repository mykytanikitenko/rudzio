//! Regression tests for the parallel-hardlimit deadlock that motivated
//! the swap from `Mutex+Condvar` to a runtime-agnostic async semaphore.
//!
//! Background: the previous primitive parked the calling OS thread on
//! `std::sync::Condvar` when the gate was full. Under workspace-wide
//! multi-suite tokio contention (every suite carrying its own runtime,
//! 16-permit default gate shared across them), workers parked on the
//! Condvar could no longer poll the futures held by permit-holders —
//! including timer / IO / spawned-subtask wakers — so the run wedged.
//! The same shape produced a documented single-thread deadlock when
//! `parallel_hardlimit < concurrency_limit`.
//!
//! Each test below builds a *separate* tokio runtime in a child OS
//! thread to drive the workload, signals completion through an
//! `mpsc::channel`, and `recv_timeout`s on the outer thread — a timeout
//! is the deadlock signal. Without the async-semaphore swap, `H1` and
//! `H2` would hang past their 5s/3s budgets; under it they finish in
//! tens of milliseconds.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rudzio::common::context::Suite;
use rudzio::parallelism::HardLimit;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        Arc, Duration, HardLimit, NonZeroUsize, build_current_thread_rt, build_multi_thread_rt,
        mpsc, thread,
    };

    /// **H1** — the reported bug. Under tokio multi-thread with a
    /// hardlimit *below* the worker count, permit-holders that
    /// `.await` real timer wakes used to deadlock the runtime: every
    /// worker either held a permit (parked on the timer waker) or was
    /// blocked at the Condvar trying to acquire the next permit. The
    /// async-semaphore primitive yields cooperatively, so the workers
    /// stay free to drive the timer driver and unblock the holders.
    #[rudzio::test]
    async fn no_deadlock_when_holders_await_timer() -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel::<()>();
        let _child = thread::Builder::new()
            .name("hardlimit-h1".into())
            .spawn(move || {
                let rt = build_multi_thread_rt(4_usize);
                rt.block_on(async {
                    let limit = Arc::new(HardLimit::with_sink(
                        NonZeroUsize::new(2_usize),
                        |_| {},
                    ));
                    let mut handles = Vec::new();
                    for _ in 0_i32..8_i32 {
                        let limit_clone = Arc::clone(&limit);
                        handles.push(::rudzio::tokio::spawn(async move {
                            let _guard = limit_clone.acquire().await;
                            ::rudzio::tokio::time::sleep(Duration::from_millis(10_u64)).await;
                        }));
                    }
                    for handle in handles {
                        let _join = handle.await;
                    }
                });
                let _send = tx.send(());
            })
            .map_err(|err| anyhow::anyhow!("spawn child: {err:?}"))?;

        match rx.recv_timeout(Duration::from_secs(5_u64)) {
            Ok(()) => Ok(()),
            Err(_) => Err(anyhow::anyhow!(
                "DEADLOCK: hardlimit + multi-thread + timer awaits did not finish in 5s"
            )),
        }
    }

    /// **H2** — the documented single-thread case. With L=1 and two
    /// tasks, the second task used to block the only worker on the
    /// Condvar; the first task's `.await sleep` could never fire. The
    /// async semaphore yields the worker, the timer fires, the first
    /// task releases, the second acquires.
    #[rudzio::test]
    async fn no_deadlock_when_hardlimit_below_current_thread_concurrency()
    -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel::<()>();
        let _child = thread::Builder::new()
            .name("hardlimit-h2".into())
            .spawn(move || {
                let rt = build_current_thread_rt();
                rt.block_on(async {
                    let limit = Arc::new(HardLimit::with_sink(
                        NonZeroUsize::new(1_usize),
                        |_| {},
                    ));
                    let mut handles = Vec::new();
                    for _ in 0_i32..2_i32 {
                        let limit_clone = Arc::clone(&limit);
                        handles.push(::rudzio::tokio::spawn(async move {
                            let _guard = limit_clone.acquire().await;
                            ::rudzio::tokio::time::sleep(Duration::from_millis(5_u64)).await;
                        }));
                    }
                    for handle in handles {
                        let _join = handle.await;
                    }
                });
                let _send = tx.send(());
            })
            .map_err(|err| anyhow::anyhow!("spawn child: {err:?}"))?;

        match rx.recv_timeout(Duration::from_secs(3_u64)) {
            Ok(()) => Ok(()),
            Err(_) => Err(anyhow::anyhow!(
                "DEADLOCK: hardlimit < concurrency on current_thread did not finish in 3s"
            )),
        }
    }

    /// **H3** — control. Pure-CPU permit-holders never hit the
    /// deadlock under either primitive: they don't yield, so they
    /// don't depend on the workers being free. Ensures the regression
    /// gate isn't producing false positives.
    #[rudzio::test]
    async fn pure_cpu_holders_finish_under_contention() -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel::<()>();
        let _child = thread::Builder::new()
            .name("hardlimit-h3".into())
            .spawn(move || {
                let rt = build_multi_thread_rt(4_usize);
                rt.block_on(async {
                    let limit = Arc::new(HardLimit::with_sink(
                        NonZeroUsize::new(2_usize),
                        |_| {},
                    ));
                    let mut handles = Vec::new();
                    for _ in 0_i32..8_i32 {
                        let limit_clone = Arc::clone(&limit);
                        handles.push(::rudzio::tokio::spawn(async move {
                            let _guard = limit_clone.acquire().await;
                            // No await — pure CPU work that the
                            // worker runs to completion before
                            // releasing the permit.
                            let mut sum = 0_u64;
                            for n in 0_u64..10_000_u64 {
                                sum = sum.wrapping_add(n);
                            }
                            ::std::hint::black_box(sum);
                        }));
                    }
                    for handle in handles {
                        let _join = handle.await;
                    }
                });
                let _send = tx.send(());
            })
            .map_err(|err| anyhow::anyhow!("spawn child: {err:?}"))?;

        match rx.recv_timeout(Duration::from_secs(3_u64)) {
            Ok(()) => Ok(()),
            Err(_) => Err(anyhow::anyhow!(
                "control test deadlocked unexpectedly — gate or harness is broken"
            )),
        }
    }
}

/// Build a fresh tokio multi-thread runtime with a fixed worker count.
/// We construct a separate runtime per test inside a child OS thread so
/// the outer rudzio runtime hosting the test is unaffected by any
/// deadlock under test.
fn build_multi_thread_rt(workers: usize) -> ::rudzio::tokio::runtime::Runtime {
    ::rudzio::tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("test rt build failed (multi-thread)")
}

/// Build a fresh tokio current-thread runtime. Same isolation rationale
/// as [`build_multi_thread_rt`].
fn build_current_thread_rt() -> ::rudzio::tokio::runtime::Runtime {
    ::rudzio::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test rt build failed (current-thread)")
}
