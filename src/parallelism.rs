//! Cross-runtime hard cap on concurrent test-body execution.
//!
//! [`HardLimit`] gates how many rudzio test bodies may be *actively polling*
//! at once, across the whole run. The mechanism is deliberately primitive:
//! [`std::sync::Mutex`] + [`std::sync::Condvar`]. No executor-specific
//! semaphores — the gate has to work identically under tokio, compio,
//! futures-executor, embassy, etc., and "honest" means a thread that can't
//! acquire really parks on a Condvar, not on a runtime-specific yield.
//!
//! The primitive is used from the generated per-test fn (see
//! `macro-internals/src/suite_codegen.rs`), where each test acquires one
//! permit before its setup/body/teardown runs and releases on drop. See
//! [`crate::config::Config::parallel_hardlimit`] for the user-facing knob
//! and its resolution rules.

use std::num::NonZeroUsize;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

/// Global hard cap on concurrent test-body execution.
///
/// Constructed once per test run and shared via [`crate::config::Config`];
/// every generated per-test fn calls [`HardLimit::acquire`] before running
/// its setup/body/teardown.
///
/// See the [module-level docs](self) for the "why real Condvar, not an
/// async semaphore" rationale.
pub struct HardLimit {
    inner: Option<Inner>,
    sink: Box<dyn Fn(&str) + Send + Sync>,
}

struct Inner {
    state: Mutex<State>,
    cvar: Condvar,
    max: NonZeroUsize,
}

struct State {
    available: usize,
}

/// RAII permit. Dropping it returns the slot to the pool and wakes one
/// waiter (if any). A guard from a disabled (`None`-mode) [`HardLimit`]
/// is a no-op on drop.
#[derive(Debug)]
pub struct HardLimitGuard<'a> {
    owner: Option<&'a HardLimit>,
}

impl HardLimit {
    /// `None` = gate disabled, acquire is a no-op. `Some(n)` = at most
    /// `n` concurrent permits; additional acquirers park until one is
    /// released.
    #[must_use]
    pub fn new(limit: Option<NonZeroUsize>) -> Self {
        Self {
            inner: limit.map(|max| Inner {
                state: Mutex::new(State {
                    available: max.get(),
                }),
                cvar: Condvar::new(),
                max,
            }),
            sink: Box::new(|s| println!("{s}")),
        }
    }

    /// Test-only constructor: pipe parking notices into a caller-provided
    /// sink instead of stdout, so assertions can inspect what was emitted.
    #[cfg(test)]
    pub(crate) fn with_sink(
        limit: Option<NonZeroUsize>,
        sink: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: limit.map(|max| Inner {
                state: Mutex::new(State {
                    available: max.get(),
                }),
                cvar: Condvar::new(),
                max,
            }),
            sink: Box::new(sink),
        }
    }

    /// Block the calling thread until a permit is available, then hand
    /// back an RAII guard. Disabled-mode returns immediately with a
    /// no-op guard.
    ///
    /// A notice is emitted to the sink (stdout in production) **only if
    /// the thread actually parked** on the Condvar — never on the
    /// fast-path. The emitted line carries the measured parking duration.
    pub fn acquire(&self) -> HardLimitGuard<'_> {
        let Some(inner) = &self.inner else {
            return HardLimitGuard { owner: None };
        };

        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        if state.available > 0 {
            state.available -= 1;
            return HardLimitGuard { owner: Some(self) };
        }

        let parked_at = Instant::now();
        let mut state = inner
            .cvar
            .wait_while(state, |s| s.available == 0)
            .unwrap_or_else(PoisonError::into_inner);
        state.available -= 1;
        drop(state);
        let parked = parked_at.elapsed();

        (self.sink)(&format!(
            "rudzio: parked {parked:?} on parallel-hardlimit ({max} max); \
             disable with --threads-parallel-hardlimit=none",
            max = inner.max.get(),
        ));

        HardLimitGuard { owner: Some(self) }
    }
}

impl std::fmt::Debug for HardLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            None => f.debug_struct("HardLimit").field("mode", &"disabled").finish(),
            Some(inner) => f
                .debug_struct("HardLimit")
                .field("max", &inner.max.get())
                .finish_non_exhaustive(),
        }
    }
}

impl Drop for HardLimitGuard<'_> {
    fn drop(&mut self) {
        let Some(owner) = self.owner else { return };
        let Some(inner) = &owner.inner else { return };
        let mut state = inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.available += 1;
        drop(state);
        inner.cvar.notify_one();
    }
}

#[cfg(test)]
mod tests {
    //! Tests use a deterministic latch primitive (`Condvar` + `Mutex<bool>`)
    //! plus mpsc acks to synchronise worker threads — no `thread::sleep`,
    //! no timing-dependent waits in the success path. `recv_timeout` is
    //! used exclusively to prove *absence* of an event (e.g. a thread must
    //! still be parked), which is a safe direction to time-bound.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::thread;
    use std::time::Duration;

    /// A one-shot boolean latch. Workers call [`Latch::wait`] to park
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

    fn collect_sink() -> (Arc<StdMutex<Vec<String>>>, impl Fn(&str) + Send + Sync + 'static) {
        let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_store = Arc::clone(&captured);
        let sink = move |line: &str| {
            let mut guard = sink_store
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard.push(line.to_owned());
        };
        (captured, sink)
    }

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN)
    }

    #[test]
    fn unlimited_mode_never_blocks_never_emits() {
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
            h.join().unwrap_or_else(|_| panic!("worker panicked"));
        }

        let out = captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(out.is_empty(), "expected no emissions, got {out:?}");
    }

    #[test]
    fn permit_count_caps_concurrent_acquires() {
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
                .unwrap_or_else(|e| panic!("ack recv failed: {e:?}"));
        }

        // At this exact moment two workers hold permits (they're parked on
        // `release.wait()`) and the other three are parked on `acquire`.
        // Peak must equal 2; the three parked workers cannot have bumped
        // it because their acquire hasn't returned.
        let peak_under_pressure = peak.load(Ordering::SeqCst);
        assert_eq!(
            peak_under_pressure, 2,
            "two permits held, but peak concurrency is {peak_under_pressure}"
        );

        // Let the held workers finish, which releases permits and wakes
        // the parked workers one-by-one.
        release.open();
        for h in handles {
            h.join().unwrap_or_else(|_| panic!("worker panicked"));
        }

        // Peak must still be exactly 2 after the full run — no race ever
        // lets the gate leak a 3rd concurrent holder.
        let final_peak = peak.load(Ordering::SeqCst);
        assert_eq!(
            final_peak, 2,
            "expected peak concurrency to stay at 2, got {final_peak}"
        );
    }

    #[test]
    fn third_acquire_blocks_and_emits_on_unblock() {
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

        // Prove the third thread is parked by showing it does NOT send
        // an ack within a bounded window. This is a time-bounded
        // *absence*-check, which is the correct direction to bound —
        // never a time-bounded presence check.
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "third acquire should still be parked"
        );
        {
            let out = captured
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            assert!(
                out.is_empty(),
                "expected no emissions while parked, got {out:?}"
            );
        }

        drop(g1);
        rx.recv()
            .unwrap_or_else(|e| panic!("third never unblocked: {e:?}"));
        third
            .join()
            .unwrap_or_else(|_| panic!("third thread panicked"));
        drop(g2);

        let out = captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(out.len(), 1, "expected exactly one emit, got {out:?}");
        let line = &out[0];
        assert!(
            line.starts_with("rudzio: parked "),
            "unexpected prefix: {line:?}"
        );
        assert!(
            line.contains("on parallel-hardlimit (2 max)"),
            "missing max marker: {line:?}"
        );
        assert!(
            line.contains("disable with --threads-parallel-hardlimit=none"),
            "missing disable hint: {line:?}"
        );
    }

    #[test]
    fn fast_path_never_emits() {
        let (captured, sink) = collect_sink();
        let limit = HardLimit::with_sink(Some(nz(4)), sink);

        for _ in 0..10 {
            let _g = limit.acquire();
        }

        // Four concurrent fast-path holders at the permit ceiling:
        // synchronise on a barrier so every thread really is simultaneously
        // holding a permit, no one parks, and no sink emission can occur.
        let limit = Arc::new(limit);
        let gate = Arc::new(std::sync::Barrier::new(4));
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
            h.join().unwrap_or_else(|_| panic!("worker panicked"));
        }

        let out = captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(out.is_empty(), "expected no emissions, got {out:?}");
    }

    #[test]
    fn guard_release_notifies_next_waiter() {
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
        assert!(
            rx.recv_timeout(Duration::from_millis(30)).is_err(),
            "waiter should be parked"
        );
        drop(g);
        rx.recv()
            .unwrap_or_else(|e| panic!("waiter never unblocked: {e:?}"));
        waiter
            .join()
            .unwrap_or_else(|_| panic!("waiter panicked"));
    }

    #[test]
    fn acquire_survives_prior_thread_panic() {
        let (_, sink) = collect_sink();
        let limit = Arc::new(HardLimit::with_sink(Some(nz(2)), sink));

        // A permit-holding thread panicking exercises the guard's
        // PoisonError::into_inner path during unwind. A subsequent
        // acquire from the main thread must succeed regardless of
        // whether the mutex ended up poisoned.
        let l_clone = Arc::clone(&limit);
        let _unused = thread::spawn(move || {
            let _g = l_clone.acquire();
            panic!("intentional");
        })
        .join();

        let _g = limit.acquire();
    }
}
