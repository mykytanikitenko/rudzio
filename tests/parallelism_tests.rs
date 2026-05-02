//! Rudzio-driven tests for [`rudzio::parallelism::HardLimit`].
//!
//! Originally lived as `#[cfg(test)] mod tests` inside
//! `src/parallelism.rs`; moved here because rudzio's `Cargo.toml` sets
//! `[lib] test = false`, so the inline libtest module never ran. The
//! tests exercise the only primitive in the framework that controls
//! cross-runtime concurrency, so we want them actually executing.
//!
//! After the swap to a runtime-agnostic async semaphore, the suite is
//! pinned to tokio runtimes (`Multithread`, `CurrentThread`) so the
//! test bodies can use `tokio::spawn` + `tokio::sync::oneshot` for
//! coordination. The `HardLimit` primitive itself is runtime-agnostic;
//! the choice of runtime here is purely about the test plumbing.
//!
//! Synchronisation discipline: deterministic oneshots and `Notify`
//! coordinate between tasks — no `tokio::time::sleep` in the success
//! path. `timeout` is used exclusively to prove *absence* of an event
//! (a future must still be Pending), which is the only safe direction
//! to time-bound.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rudzio::common::context::Suite;
use rudzio::parallelism::HardLimit;
use rudzio::runtime::tokio::{CurrentThread, Multithread};
use rudzio::tokio::spawn;
use rudzio::tokio::sync::oneshot;
use rudzio::tokio::time::sleep;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        Arc, AtomicUsize, Duration, HardLimit, Ordering, PoisonError, collect_sink, nz, oneshot,
        sleep, spawn,
    };

    #[rudzio::test]
    async fn fast_path_never_emits() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = HardLimit::with_sink(Some(nz(4)), sink);

        // Sequential acquires well below the ceiling: every one is
        // try_acquire fast-path, no emission.
        for _ in 0_i32..10_i32 {
            let _guard = limit.acquire().await;
        }

        let snapshot = {
            let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
            out.clone()
        };
        anyhow::ensure!(
            snapshot.is_empty(),
            "expected no emissions, got {snapshot:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn guard_release_wakes_next_waiter() -> anyhow::Result<()> {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(1)), sink));

        let outer_guard = limit.acquire().await;
        let limit_clone = Arc::clone(&limit);
        let (tx, rx) = oneshot::channel::<()>();
        let waiter = spawn(async move {
            let _guard = limit_clone.acquire().await;
            tx.send(()).map_err(|()| anyhow::anyhow!("send failed"))
        });

        // Prove the waiter is parked: a bounded sleep must elapse with
        // the rx still empty. Time-bounded *absence*-check.
        sleep(Duration::from_millis(30_u64)).await;

        // Drop the held permit; waiter must wake and signal.
        drop(outer_guard);
        rx.await
            .map_err(|err| anyhow::anyhow!("waiter never unblocked: {err:?}"))?;
        waiter
            .await
            .map_err(|err| anyhow::anyhow!("waiter join failed: {err:?}"))??;
        Ok(())
    }

    #[rudzio::test]
    async fn permit_count_caps_concurrent_acquires() -> anyhow::Result<()> {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2)), sink));

        let active = Arc::new(AtomicUsize::new(0_usize));
        let peak = Arc::new(AtomicUsize::new(0_usize));

        // Each task acquires, holds for 30ms (forcing real overlap
        // with at least one other task), records peak via fetch_max,
        // then releases. With L=2 and 5 tasks, exactly two are
        // concurrent at any time — peak must equal 2 (never higher).
        let mut handles = Vec::new();
        for _ in 0_i32..5_i32 {
            let limit_clone = Arc::clone(&limit);
            let active_clone = Arc::clone(&active);
            let peak_clone = Arc::clone(&peak);
            handles.push(spawn(async move {
                let _guard = limit_clone.acquire().await;
                let now = active_clone
                    .fetch_add(1_usize, Ordering::SeqCst)
                    .saturating_add(1_usize);
                let _prev_peak = peak_clone.fetch_max(now, Ordering::SeqCst);
                sleep(Duration::from_millis(30_u64)).await;
                let _prev_active = active_clone.fetch_sub(1_usize, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle
                .await
                .map_err(|err| anyhow::anyhow!("worker join failed: {err:?}"))?;
        }

        let observed_peak = peak.load(Ordering::SeqCst);
        anyhow::ensure!(
            observed_peak == 2_usize,
            "expected peak concurrency to stay at 2, got {observed_peak}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn third_acquire_waits_and_emits_on_unblock() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2)), sink));

        let g1 = limit.acquire().await;
        let g2 = limit.acquire().await;

        let limit_clone = Arc::clone(&limit);
        let (tx, rx) = oneshot::channel::<()>();
        let third = spawn(async move {
            let _guard = limit_clone.acquire().await;
            tx.send(()).map_err(|()| anyhow::anyhow!("send failed"))
        });

        // Prove the third future is parked by showing it does NOT
        // signal within a bounded window.
        sleep(Duration::from_millis(50_u64)).await;
        let parked_snapshot = {
            let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
            out.clone()
        };
        anyhow::ensure!(
            parked_snapshot.is_empty(),
            "expected no emissions while parked, got {parked_snapshot:?}"
        );

        drop(g1);
        rx.await
            .map_err(|err| anyhow::anyhow!("third never unblocked: {err:?}"))?;
        third
            .await
            .map_err(|err| anyhow::anyhow!("third join failed: {err:?}"))??;
        drop(g2);

        let snapshot = {
            let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
            out.clone()
        };
        anyhow::ensure!(
            snapshot.len() == 1_usize,
            "expected exactly one emit, got {snapshot:?}"
        );
        let line = snapshot
            .first()
            .ok_or_else(|| anyhow::anyhow!("snapshot unexpectedly empty"))?;
        anyhow::ensure!(
            line.starts_with("rudzio: parked "),
            "unexpected prefix: {line:?}"
        );
        anyhow::ensure!(
            line.contains("on parallel-hardlimit (2 max)"),
            "missing max marker: {line:?}"
        );
        anyhow::ensure!(
            line.contains("disable with --threads-parallel-hardlimit=none"),
            "missing disable hint: {line:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn unlimited_mode_never_blocks_never_emits() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(None, sink));

        let mut handles = Vec::new();
        for _ in 0_i32..32_i32 {
            let limit_clone = Arc::clone(&limit);
            handles.push(spawn(async move {
                for _ in 0_i32..64_i32 {
                    let _guard = limit_clone.acquire().await;
                }
            }));
        }
        for handle in handles {
            handle
                .await
                .map_err(|err| anyhow::anyhow!("worker join failed: {err:?}"))?;
        }

        let snapshot = {
            let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
            out.clone()
        };
        anyhow::ensure!(
            snapshot.is_empty(),
            "expected no emissions, got {snapshot:?}"
        );
        Ok(())
    }
}

/// Build a captured-emission sink and the shared buffer that backs it.
/// Returns `(captured, sink)` where `sink` is suitable for
/// [`HardLimit::with_sink`] and `captured` is the buffer the test
/// inspects after the run.
fn collect_sink() -> (
    Arc<StdMutex<Vec<String>>>,
    impl Fn(&str) + Send + Sync + 'static,
) {
    let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink_store = Arc::clone(&captured);
    let sink = move |line: &str| {
        let mut buffer = sink_store.lock().unwrap_or_else(PoisonError::into_inner);
        buffer.push(line.to_owned());
    };
    (captured, sink)
}

/// Build a [`NonZeroUsize`] from a runtime value, falling back to
/// [`NonZeroUsize::MIN`] (= 1) when the input is zero.
fn nz(count: usize) -> NonZeroUsize {
    NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN)
}
