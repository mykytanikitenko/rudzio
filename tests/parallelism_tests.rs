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

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Condvar, Mutex as StdMutex, PoisonError};
use std::thread;
use std::time::Duration;

use rudzio::parallelism::HardLimit;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::Local::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::embassy::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::futures::ThreadPool::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::{
        Arc, AtomicUsize, Barrier, Duration, HardLimit, Latch, Ordering, PoisonError, collect_sink,
        mpsc, nz, thread,
    };

    #[rudzio::test]
    fn unlimited_mode_never_blocks_never_emits() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(None, sink));

        let handles: Vec<_> = (0..32)
            .map(|_| {
                let l = Arc::clone(&limit);
                thread::spawn(move || {
                    for _ in 0..64 {
                        let _g = l.acquire();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("worker panicked"))?;
        }

        let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
        anyhow::ensure!(out.is_empty(), "expected no emissions, got {out:?}");
        Ok(())
    }

    #[rudzio::test]
    fn permit_count_caps_concurrent_acquires() -> anyhow::Result<()> {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2)), sink));

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Latch::default());
        let (ack_tx, ack_rx) = mpsc::channel::<()>();

        let handles: Vec<_> = (0..5)
            .map(|_| {
                let l = Arc::clone(&limit);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let release = Arc::clone(&release);
                let ack_tx = ack_tx.clone();
                thread::spawn(move || {
                    let _g = l.acquire();
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    let _prev_peak = peak.fetch_max(now, Ordering::SeqCst);
                    // Announce "I hold a permit" before parking on the
                    // release latch — main uses this to know when two
                    // permits are concurrently held and it's safe to
                    // inspect peak without a time-based wait.
                    ack_tx
                        .send(())
                        .unwrap_or_else(|_| panic!("ack send failed"));
                    release.wait();
                    let _prev_active = active.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        drop(ack_tx);

        // Deterministic wait for `permit_limit` = 2 simultaneous holders.
        for _ in 0..2 {
            ack_rx
                .recv()
                .map_err(|e| anyhow::anyhow!("ack recv failed: {e:?}"))?;
        }

        // At this exact moment two workers hold permits (they're parked
        // on `release.wait()`) and the other three are parked on
        // `acquire`. Peak must equal 2; the three parked workers can't
        // have bumped it because their acquire hasn't returned.
        let peak_under_pressure = peak.load(Ordering::SeqCst);
        anyhow::ensure!(
            peak_under_pressure == 2,
            "two permits held, but peak concurrency is {peak_under_pressure}"
        );

        // Let the held workers finish, releasing permits and waking
        // the parked workers one-by-one.
        release.open();
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("worker panicked"))?;
        }

        // Peak must still be exactly 2 after the full run — no race
        // ever lets the gate leak a 3rd concurrent holder.
        let final_peak = peak.load(Ordering::SeqCst);
        anyhow::ensure!(
            final_peak == 2,
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
            let l = Arc::clone(&limit);
            thread::spawn(move || {
                let _g = l.acquire();
                tx.send(()).unwrap_or_else(|_| panic!("send failed"));
            })
        };

        // Prove the third thread is parked by showing it does NOT
        // send an ack within a bounded window. Time-bounded
        // *absence*-check, the only direction that's safe to bound.
        anyhow::ensure!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "third acquire should still be parked"
        );
        {
            let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
            anyhow::ensure!(
                out.is_empty(),
                "expected no emissions while parked, got {out:?}"
            );
        }

        drop(g1);
        rx.recv()
            .map_err(|e| anyhow::anyhow!("third never unblocked: {e:?}"))?;
        third
            .join()
            .map_err(|_| anyhow::anyhow!("third thread panicked"))?;
        drop(g2);

        let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
        anyhow::ensure!(out.len() == 1, "expected exactly one emit, got {out:?}");
        let line = &out[0];
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
    fn fast_path_never_emits() -> anyhow::Result<()> {
        let (captured, sink) = collect_sink();
        let limit = HardLimit::with_sink(Some(nz(4)), sink);

        for _ in 0..10 {
            let _g = limit.acquire();
        }

        // Four concurrent fast-path holders at the permit ceiling:
        // synchronise on a barrier so every thread really is
        // simultaneously holding a permit, no one parks, no sink
        // emission can occur.
        let limit = Arc::new(limit);
        let gate = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let l = Arc::clone(&limit);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    let _g = l.acquire();
                    let _wait_result = gate.wait();
                })
            })
            .collect();
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("worker panicked"))?;
        }

        let out = captured.lock().unwrap_or_else(PoisonError::into_inner);
        anyhow::ensure!(out.is_empty(), "expected no emissions, got {out:?}");
        Ok(())
    }

    #[rudzio::test]
    fn guard_release_notifies_next_waiter() -> anyhow::Result<()> {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(1)), sink));

        let g = limit.acquire();
        let (tx, rx) = mpsc::channel::<()>();
        let waiter = {
            let l = Arc::clone(&limit);
            thread::spawn(move || {
                let _g = l.acquire();
                tx.send(()).unwrap_or_else(|_| panic!("send failed"));
            })
        };
        anyhow::ensure!(
            rx.recv_timeout(Duration::from_millis(30)).is_err(),
            "waiter should be parked"
        );
        drop(g);
        rx.recv()
            .map_err(|e| anyhow::anyhow!("waiter never unblocked: {e:?}"))?;
        waiter
            .join()
            .map_err(|_| anyhow::anyhow!("waiter panicked"))?;
        Ok(())
    }

    #[rudzio::test]
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
        let test_id = ::rudzio::output::TestId::next();
        let l_clone = Arc::clone(&limit);
        let _unused = thread::spawn(move || {
            ::rudzio::output::panic_hook::set_current_test(Some(test_id));
            let _g = l_clone.acquire();
            panic!("intentional");
        })
        .join();

        let _g = limit.acquire();
        Ok(())
    }
}

// --- helpers (file-level so the inner mod can `use super::*`) ---

/// One-shot boolean latch. Workers call [`Latch::wait`] to park
/// until the main thread calls [`Latch::open`], at which point
/// everyone (current waiters and future callers) proceeds.
#[derive(Default)]
struct Latch {
    lock: StdMutex<bool>,
    cvar: Condvar,
}

impl Latch {
    fn open(&self) {
        let mut flag = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        *flag = true;
        drop(flag);
        self.cvar.notify_all();
    }

    fn wait(&self) {
        let flag = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let flag = self
            .cvar
            .wait_while(flag, |open| !*open)
            .unwrap_or_else(PoisonError::into_inner);
        drop(flag);
    }
}

fn collect_sink() -> (
    Arc<StdMutex<Vec<String>>>,
    impl Fn(&str) + Send + Sync + 'static,
) {
    let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink_store = Arc::clone(&captured);
    let sink = move |line: &str| {
        let mut guard = sink_store.lock().unwrap_or_else(PoisonError::into_inner);
        guard.push(line.to_owned());
    };
    (captured, sink)
}

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN)
}
