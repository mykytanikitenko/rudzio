//! Rudzio-driven tests for [`rudzio::parallelism::HardLimit`].
//!
//! Originally lived as `#[cfg(test)] mod tests` inside
//! `src/parallelism.rs`. Moved here because rudzio's `Cargo.toml`
//! sets `[lib] test = false`, so the inline libtest module never
//! ran under either `cargo test` or `cargo rudzio test`. The tests
//! exercise the only primitive in the framework that controls
//! cross-runtime concurrency, so we want them actually executing.
//!
//! Synchronisation discipline (preserved from the original):
//! deterministic latch (`Condvar` + `Mutex<bool>`) plus mpsc acks
//! to coordinate worker threads — no `thread::sleep`, no
//! timing-dependent waits in the success path. `recv_timeout` is
//! used exclusively to prove *absence* of an event (a thread must
//! still be parked), which is the only safe direction to time-bound.

use std::any::Any;
use std::iter;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Condvar, Mutex as StdMutex, PoisonError};
use std::thread;
use std::time::Duration;

use std::panic::panic_any;

use rudzio::common::context::Suite;
use rudzio::output::TestId;
use rudzio::output::panic_hook;
use rudzio::parallelism::HardLimit;
use rudzio::runtime::compio;
use rudzio::runtime::embassy;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        Any, Arc, AtomicUsize, Barrier, Duration, HardLimit, Latch, Ordering, PoisonError, TestId,
        collect_sink, iter, join_thread, mpsc, nz, panic_any, panic_hook, thread,
    };

    #[rudzio::test]
    #[expect(
        clippy::panic,
        reason = "this test asserts HardLimit::acquire survives a previous permit-holder thread panicking and poisoning the underlying std::sync::Mutex; the only way to poison std::sync::Mutex on stable Rust is to actually unwind through a held guard, so a real panic is required as the system-under-test trigger"
    )]
    fn acquire_survives_prior_thread_panic() -> anyhow::Result<()> {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2)), sink));

        // A permit-holding thread panicking exercises the guard's
        // PoisonError::into_inner path during unwind. A subsequent
        // acquire from the main thread must succeed regardless of
        // whether the mutex ended up poisoned.
        //
        // The spawned thread tags itself as belonging to this test
        // (via the framework's `set_current_test` thread-local) so
        // the rudzio panic_hook attributes the intentional panic to
        // a test boundary instead of bumping the unattributed-panic
        // counter — without this the safety net would mark the run
        // FAILED at the end even though the test itself passed.
        let test_id = TestId::next();
        let limit_clone = Arc::clone(&limit);
        let join_result: Result<(), Box<dyn Any + Send>> = thread::spawn(move || {
            panic_hook::set_current_test(Some(test_id));
            let _guard = limit_clone.acquire();
            panic_any("intentional");
        })
        .join();

        anyhow::ensure!(
            join_result.is_err(),
            "spawned thread should have panicked but did not"
        );

        let _guard = limit.acquire();
        Ok(())
    }

    #[rudzio::test]
    fn fast_path_never_emits() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = HardLimit::with_sink(Some(nz(4)), sink);

        for _ in 0_i32..10_i32 {
            let _guard = limit.acquire();
        }

        // Four concurrent fast-path holders at the permit ceiling:
        // synchronise on a barrier so every thread really is
        // simultaneously holding a permit, no one parks, no sink
        // emission can occur.
        let limit_arc = Arc::new(limit);
        let gate = Arc::new(Barrier::new(4_usize));
        let handles: Vec<_> = iter::repeat_with(|| {
            let limit_clone = Arc::clone(&limit_arc);
            let gate_clone = Arc::clone(&gate);
            thread::spawn(move || {
                let _guard = limit_clone.acquire();
                let _wait_result = gate_clone.wait();
            })
        })
        .take(4_usize)
        .collect();
        for handle in handles {
            join_thread(handle, "worker panicked")?;
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
    fn guard_release_notifies_next_waiter() -> anyhow::Result<()> {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(1)), sink));

        let outer_guard = limit.acquire();
        let (tx, rx) = mpsc::channel::<()>();
        let waiter = {
            let limit_clone = Arc::clone(&limit);
            thread::spawn(move || -> anyhow::Result<()> {
                let _guard = limit_clone.acquire();
                tx.send(())
                    .map_err(|err| anyhow::anyhow!("send failed: {err:?}"))
            })
        };
        anyhow::ensure!(
            rx.recv_timeout(Duration::from_millis(30_u64)).is_err(),
            "waiter should be parked"
        );
        drop(outer_guard);
        rx.recv()
            .map_err(|err| anyhow::anyhow!("waiter never unblocked: {err:?}"))?;
        join_thread(waiter, "waiter panicked")??;
        Ok(())
    }

    #[rudzio::test]
    fn permit_count_caps_concurrent_acquires() -> anyhow::Result<()> {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2)), sink));

        let active = Arc::new(AtomicUsize::new(0_usize));
        let peak = Arc::new(AtomicUsize::new(0_usize));
        let release = Arc::new(Latch::default());
        let (ack_tx, ack_rx) = mpsc::channel::<()>();

        let handles: Vec<_> = iter::repeat_with(|| {
            let limit_clone = Arc::clone(&limit);
            let active_clone = Arc::clone(&active);
            let peak_clone = Arc::clone(&peak);
            let release_clone = Arc::clone(&release);
            let ack_tx_clone = ack_tx.clone();
            thread::spawn(move || -> anyhow::Result<()> {
                let _guard = limit_clone.acquire();
                let now = active_clone
                    .fetch_add(1_usize, Ordering::SeqCst)
                    .saturating_add(1_usize);
                let _prev_peak = peak_clone.fetch_max(now, Ordering::SeqCst);
                // Announce "I hold a permit" before parking on the
                // release latch — main uses this to know when two
                // permits are concurrently held and it's safe to
                // inspect peak without a time-based wait.
                ack_tx_clone
                    .send(())
                    .map_err(|err| anyhow::anyhow!("ack send failed: {err:?}"))?;
                release_clone.wait();
                let _prev_active = active_clone.fetch_sub(1_usize, Ordering::SeqCst);
                Ok(())
            })
        })
        .take(5_usize)
        .collect();
        drop(ack_tx);

        // Deterministic wait for `permit_limit` = 2 simultaneous holders.
        for _ in 0_i32..2_i32 {
            ack_rx
                .recv()
                .map_err(|err| anyhow::anyhow!("ack recv failed: {err:?}"))?;
        }

        // At this exact moment two workers hold permits (they're parked
        // on `release.wait()`) and the other three are parked on
        // `acquire`. Peak must equal 2; the three parked workers can't
        // have bumped it because their acquire hasn't returned.
        let peak_under_pressure = peak.load(Ordering::SeqCst);
        anyhow::ensure!(
            peak_under_pressure == 2_usize,
            "two permits held, but peak concurrency is {peak_under_pressure}"
        );

        // Let the held workers finish, releasing permits and waking
        // the parked workers one-by-one.
        release.open();
        for handle in handles {
            join_thread(handle, "worker panicked")??;
        }

        // Peak must still be exactly 2 after the full run — no race
        // ever lets the gate leak a 3rd concurrent holder.
        let final_peak = peak.load(Ordering::SeqCst);
        anyhow::ensure!(
            final_peak == 2_usize,
            "expected peak concurrency to stay at 2, got {final_peak}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn third_acquire_blocks_and_emits_on_unblock() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2)), sink));

        let g1 = limit.acquire();
        let g2 = limit.acquire();

        let (tx, rx) = mpsc::channel::<()>();
        let third = {
            let limit_clone = Arc::clone(&limit);
            thread::spawn(move || -> anyhow::Result<()> {
                let _guard = limit_clone.acquire();
                tx.send(())
                    .map_err(|err| anyhow::anyhow!("send failed: {err:?}"))
            })
        };

        // Prove the third thread is parked by showing it does NOT
        // send an ack within a bounded window. Time-bounded
        // *absence*-check, the only direction that's safe to bound.
        anyhow::ensure!(
            rx.recv_timeout(Duration::from_millis(50_u64)).is_err(),
            "third acquire should still be parked"
        );
        let parked_snapshot = {
            let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
            out.clone()
        };
        anyhow::ensure!(
            parked_snapshot.is_empty(),
            "expected no emissions while parked, got {parked_snapshot:?}"
        );

        drop(g1);
        rx.recv()
            .map_err(|err| anyhow::anyhow!("third never unblocked: {err:?}"))?;
        join_thread(third, "third thread panicked")??;
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
    fn unlimited_mode_never_blocks_never_emits() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(None, sink));

        let handles: Vec<_> = iter::repeat_with(|| {
            let limit_clone = Arc::clone(&limit);
            thread::spawn(move || {
                for _ in 0_i32..64_i32 {
                    let _guard = limit_clone.acquire();
                }
            })
        })
        .take(32_usize)
        .collect();
        for handle in handles {
            join_thread(handle, "worker panicked")?;
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

// --- helpers (file-level so the inner mod can `use super::*`) ---

/// One-shot boolean latch. Workers call [`Latch::wait`] to park
/// until the main thread calls [`Latch::open`], at which point
/// everyone (current waiters and future callers) proceeds.
#[derive(Default)]
struct Latch {
    /// Condvar used to park waiters until `lock` flips to `true`.
    cvar: Condvar,
    /// Flag protected by the condvar; once `true`, waiters proceed.
    lock: StdMutex<bool>,
}

impl Latch {
    /// Release every parked waiter and any future callers of
    /// [`Latch::wait`].
    fn open(&self) {
        let mut flag_guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        *flag_guard = true;
        drop(flag_guard);
        self.cvar.notify_all();
    }

    /// Park the calling thread until [`Latch::open`] is called.
    /// Returns immediately if the latch is already open.
    fn wait(&self) {
        let initial_guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let final_guard = self
            .cvar
            .wait_while(initial_guard, |open| !*open)
            .unwrap_or_else(PoisonError::into_inner);
        drop(final_guard);
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

/// Join `handle` and translate a thread panic into an `anyhow::Error`
/// carrying the panic payload's message (best-effort string extraction).
fn join_thread<T>(handle: thread::JoinHandle<T>, label: &'static str) -> anyhow::Result<T> {
    handle.join().map_err(|payload| {
        let message = payload
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic>");
        anyhow::anyhow!("{label}: {message}")
    })
}

/// Build a [`NonZeroUsize`] from a runtime value, falling back to
/// [`NonZeroUsize::MIN`] (= 1) when the input is zero.
fn nz(count: usize) -> NonZeroUsize {
    NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN)
}
