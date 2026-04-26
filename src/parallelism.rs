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

use std::fmt;
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
    /// Backing slot pool; `None` means the gate is disabled and every
    /// acquire is a no-op fast-path.
    inner: Option<Inner>,
    /// Sink for one-line parking notices (production prints to stdout;
    /// tests inject their own).
    sink: Box<dyn Fn(&str) + Send + Sync>,
}

/// Backing pool shared by every [`HardLimitGuard`] — the mutex/condvar
/// pair plus the configured maximum.
struct Inner {
    /// Condvar woken when a permit is released so a parked acquirer
    /// can resume.
    cvar: Condvar,
    /// Configured maximum number of concurrent permits.
    max: NonZeroUsize,
    /// Mutable per-pool state behind a lock.
    state: Mutex<State>,
}

/// Mutable counters guarded by `Inner::state`.
struct State {
    /// Number of permits currently free.
    available: usize,
}

/// RAII permit. Dropping it returns the slot to the pool and wakes one
/// waiter (if any). A guard from a disabled (`None`-mode) [`HardLimit`]
/// is a no-op on drop.
#[derive(Debug)]
pub struct HardLimitGuard<'gate> {
    /// Owning gate when this guard holds a real permit; `None` for
    /// no-op guards from disabled-mode acquires.
    owner: Option<&'gate HardLimit>,
}

impl HardLimit {
    /// Block the calling thread until a permit is available, then hand
    /// back an RAII guard. Disabled-mode returns immediately with a
    /// no-op guard.
    ///
    /// A notice is emitted to the sink (stdout in production) **only if
    /// the thread actually parked** on the Condvar — never on the
    /// fast-path. The emitted line carries the measured parking duration.
    #[inline]
    pub fn acquire(&self) -> HardLimitGuard<'_> {
        let Some(inner) = &self.inner else {
            return HardLimitGuard { owner: None };
        };

        let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);

        if state.available > 0 {
            state.available -= 1;
            return HardLimitGuard { owner: Some(self) };
        }

        let parked_at = Instant::now();
        let mut state = inner
            .cvar
            .wait_while(state, |inner| inner.available == 0)
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

    /// `None` = gate disabled, acquire is a no-op. `Some(n)` = at most
    /// `n` concurrent permits; additional acquirers park until one is
    /// released.
    #[must_use]
    #[inline]
    pub fn new(limit: Option<NonZeroUsize>) -> Self {
        Self {
            inner: limit.map(|max| Inner {
                state: Mutex::new(State {
                    available: max.get(),
                }),
                cvar: Condvar::new(),
                max,
            }),
            sink: Box::new(|msg| println!("{msg}")),
        }
    }

    /// Constructor that pipes parking notices into a caller-provided
    /// sink instead of stdout — used by the in-tree integration tests
    /// in `tests/parallelism_tests.rs` to capture what was emitted.
    /// Hidden from the rendered docs because the production
    /// constructor [`HardLimit::new`] is the only stable surface for
    /// downstream users.
    #[doc(hidden)]
    #[inline]
    pub fn with_sink(
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
}

impl fmt::Debug for HardLimit {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            None => f
                .debug_struct("HardLimit")
                .field("mode", &"disabled")
                .finish(),
            Some(inner) => f
                .debug_struct("HardLimit")
                .field("max", &inner.max.get())
                .finish_non_exhaustive(),
        }
    }
}

impl Drop for HardLimitGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        let Some(owner) = self.owner else { return };
        let Some(inner) = &owner.inner else { return };
        let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.available += 1;
        drop(state);
        inner.cvar.notify_one();
    }
}

// Unit tests live as proper rudzio tests in
// `tests/parallelism_tests.rs`. The previous `#[cfg(test)] mod tests`
// here never ran — `[lib] test = false` in `Cargo.toml` disables the
// libtest target — and was invisible to `cargo rudzio test` for the
// same reason. The dedicated test crate dogfoods the framework and
// gives the suite a place to live.
