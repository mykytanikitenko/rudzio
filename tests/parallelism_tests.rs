//! Rudzio-driven tests for [`rudzio::parallelism::HardLimit`].
//!
//! Originally lived as `#[cfg(test)] mod tests` inside
//! `src/parallelism.rs`; moved here because rudzio's `Cargo.toml` sets
//! `[lib] test = false`, so the inline libtest module never ran. The
//! tests exercise the only primitive in the framework that controls
//! cross-runtime concurrency, so we want them actually executing.
//!
//! Runtime coverage: the suite is dispatched on every supported
//! adapter (tokio mt/ct/local, compio, embassy, `futures::ThreadPool`)
//! since `HardLimit` is itself runtime-agnostic and must hold the
//! same contract everywhere. Test plumbing avoids tokio-specific
//! helpers (`tokio::spawn`, `tokio::sync::oneshot`,
//! `tokio::time::sleep`): concurrency goes through `FuturesUnordered`
//! driven by the test body, parked-state assertions use
//! `futures_util::future::poll_immediate` (single-poll probe), and
//! cooperative yielding goes through `ctx.yield_now()`.
//!
//! Synchronisation discipline: deterministic atomics + `poll_immediate`
//! coordinate between sub-futures — no wall-clock sleeps in either the
//! success path or the absence-of-event path.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rudzio::common::context::{Suite, Test};
use rudzio::parallelism::HardLimit;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use rudzio::context::Test as _;
    use rudzio::futures_util::future::poll_immediate;
    use rudzio::futures_util::stream::{FuturesUnordered, StreamExt as _};

    use super::{
        Arc, AtomicBool, AtomicUsize, HardLimit, Ordering, PoisonError, Test, collect_sink, nz,
    };

    /// Sequential acquires below the ceiling never produce a parked
    /// emission — the gate's fast path returns immediately and the
    /// sink stays untouched.
    #[rudzio::test]
    async fn fast_path_never_emits(_ctx: &Test) -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = HardLimit::with_sink(Some(nz(4_usize)), sink);

        for _idx in 0_i32..10_i32 {
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

    /// A second acquirer parks while the only permit is held, then
    /// wakes and acquires once the holder drops its guard. Parked
    /// state is verified via `poll_immediate` (single-poll probe
    /// returns None on Pending) — no wall-clock sleeps anywhere.
    #[rudzio::test]
    async fn guard_release_wakes_next_waiter(ctx: &Test) -> anyhow::Result<()> {
        let (_unused, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(1_usize)), sink));

        let outer_guard = limit.acquire().await;
        let acquired = Arc::new(AtomicBool::new(false));
        let limit_clone = Arc::clone(&limit);
        let acquired_clone = Arc::clone(&acquired);
        let mut waiters = FuturesUnordered::new();
        waiters.push(async move {
            let _guard = limit_clone.acquire().await;
            acquired_clone.store(true, Ordering::SeqCst);
        });

        // Probe: poll the waiter once. While the gate is full it must
        // stay Pending and the acquired flag must remain false.
        for _attempt in 0_i32..3_i32 {
            let probe = poll_immediate(waiters.next()).await;
            anyhow::ensure!(
                probe.is_none(),
                "waiter completed while gate full: {probe:?}"
            );
            ctx.yield_now().await;
        }
        anyhow::ensure!(
            !acquired.load(Ordering::SeqCst),
            "waiter set acquired flag while gate was full"
        );

        drop(outer_guard);
        let drained = waiters.next().await;
        anyhow::ensure!(drained.is_some(), "waiter never unblocked");
        anyhow::ensure!(
            acquired.load(Ordering::SeqCst),
            "waiter completed without setting acquired flag"
        );
        Ok(())
    }

    /// Five contenders, two permits, each holder yields cooperatively
    /// while holding so other futures can be polled. Peak concurrency
    /// (atomic `fetch_max`) must equal 2 — the gate must enforce its
    /// ceiling and never let a third holder in.
    #[rudzio::test]
    async fn permit_count_caps_concurrent_acquires(ctx: &Test) -> anyhow::Result<()> {
        let (_unused, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2_usize)), sink));

        let active = Arc::new(AtomicUsize::new(0_usize));
        let peak = Arc::new(AtomicUsize::new(0_usize));

        let mut workers = FuturesUnordered::new();
        for _idx in 0_i32..5_i32 {
            let limit_clone = Arc::clone(&limit);
            let active_clone = Arc::clone(&active);
            let peak_clone = Arc::clone(&peak);
            workers.push(async move {
                let _guard = limit_clone.acquire().await;
                let now = active_clone
                    .fetch_add(1_usize, Ordering::SeqCst)
                    .saturating_add(1_usize);
                let _prev_peak = peak_clone.fetch_max(now, Ordering::SeqCst);
                // Yield several times so the executor can poll waiting
                // sub-futures and force genuine overlap with the second
                // permit holder.
                for _yield_idx in 0_i32..5_i32 {
                    ctx.yield_now().await;
                }
                let _prev_active = active_clone.fetch_sub(1_usize, Ordering::SeqCst);
            });
        }
        while workers.next().await.is_some() {}

        let observed_peak = peak.load(Ordering::SeqCst);
        anyhow::ensure!(
            observed_peak == 2_usize,
            "expected peak concurrency to stay at 2, got {observed_peak}"
        );
        Ok(())
    }

    /// Two permits held; a third acquirer parks and triggers exactly
    /// one diagnostic emission. Releasing one of the held permits
    /// wakes the third. Parked state verified via `poll_immediate` so
    /// the test stays deterministic and runtime-agnostic.
    #[rudzio::test]
    async fn third_acquire_waits_and_emits_on_unblock(ctx: &Test) -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2_usize)), sink));

        let g1 = limit.acquire().await;
        let g2 = limit.acquire().await;

        let limit_clone = Arc::clone(&limit);
        let acquired = Arc::new(AtomicBool::new(false));
        let acquired_clone = Arc::clone(&acquired);
        let mut waiters = FuturesUnordered::new();
        waiters.push(async move {
            let _guard = limit_clone.acquire().await;
            acquired_clone.store(true, Ordering::SeqCst);
        });

        // Park-state probe: single-poll the third acquirer; it must
        // stay Pending until a held permit is released.
        for _attempt in 0_i32..3_i32 {
            let probe = poll_immediate(waiters.next()).await;
            anyhow::ensure!(
                probe.is_none(),
                "third acquirer completed while gate was full: {probe:?}"
            );
            ctx.yield_now().await;
        }
        let parked_snapshot = {
            let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
            out.clone()
        };
        anyhow::ensure!(
            parked_snapshot.is_empty(),
            "expected no emissions while still parked (sink fires on unblock, \
             not on park entry), got {parked_snapshot:?}"
        );

        drop(g1);
        let drained = waiters.next().await;
        anyhow::ensure!(drained.is_some(), "third never unblocked");
        anyhow::ensure!(
            acquired.load(Ordering::SeqCst),
            "third completed without setting acquired flag"
        );
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

    /// `None` ceiling = unlimited mode: many workers each making many
    /// acquires never produce a parked emission. The fast path stays
    /// dominant because the gate never reports full.
    #[rudzio::test]
    async fn unlimited_mode_never_blocks_never_emits(_ctx: &Test) -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(None, sink));

        let mut workers = FuturesUnordered::new();
        for _outer in 0_i32..32_i32 {
            let limit_clone = Arc::clone(&limit);
            workers.push(async move {
                for _inner in 0_i32..64_i32 {
                    let _guard = limit_clone.acquire().await;
                }
            });
        }
        while workers.next().await.is_some() {}

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
