//! Cross-runtime hard cap on concurrent test-body execution.
//!
//! [`HardLimit`] gates how many rudzio test bodies may be *actively polling*
//! at once, across the whole run. The mechanism is a runtime-agnostic
//! async semaphore ([`futures_intrusive::sync::Semaphore`]): when the gate
//! is full, the calling task `.await`s and yields control back to the
//! runtime instead of parking the OS thread. That way, permit-holders
//! whose bodies await timers / IO / spawned subtasks remain pollable —
//! historic deadlocks under multi-suite tokio contention and on
//! single-thread runtimes (when `parallel_hardlimit < concurrency_limit`)
//! are gone.
//!
//! Works identically under tokio (multi-thread / current-thread / local),
//! compio, embassy, futures-executor, and any future runtime that polls
//! futures and honors wakers. No executor-specific code path.
//!
//! The primitive is used from the generated per-test fn (see
//! `macro-internals/src/suite_codegen.rs`), where each test acquires one
//! permit before its setup/body/teardown runs and releases on drop. See
//! [`crate::config::Config::parallel_hardlimit`] for the user-facing knob
//! and its resolution rules.

use std::fmt;
use std::num::NonZeroUsize;
use std::time::Instant;

use futures_intrusive::sync::{Semaphore, SemaphoreReleaser};

use crate::output::write_stdout;

/// Global hard cap on concurrent test-body execution.
///
/// Constructed once per test run and shared via [`crate::config::Config`];
/// every generated per-test fn calls [`HardLimit::acquire`] before running
/// its setup/body/teardown.
///
/// See the [module-level docs](self) for the "why an async semaphore, not
/// a Condvar" rationale.
pub struct HardLimit {
    /// Backing semaphore + ceiling; `None` means the gate is disabled and
    /// every acquire is a no-op fast-path.
    inner: Option<Inner>,
    /// Sink for one-line parking notices (production prints to stdout;
    /// tests inject their own).
    sink: Box<dyn Fn(&str) + Send + Sync>,
}

/// Backing semaphore shared by every [`HardLimitGuard`] plus the
/// configured maximum.
struct Inner {
    /// Configured maximum number of concurrent permits.
    max: NonZeroUsize,
    /// Fair (FIFO) async semaphore; `try_acquire` is the fast-path,
    /// `acquire` is the awaitable slow path that yields when the gate
    /// is full.
    sem: Semaphore,
}

/// RAII permit returned by [`HardLimit::acquire`].
///
/// Dropping it returns the slot to the pool and wakes one waiter (if
/// any) — both happen synchronously inside the releaser's own `Drop`. A
/// guard from a disabled (`None`-mode) [`HardLimit`] is a no-op on drop.
pub struct HardLimitGuard<'gate> {
    /// `Some(_)` when this guard holds a real permit; `None` for no-op
    /// guards from disabled-mode acquires. The releaser's own `Drop`
    /// handles release + waker wakeup.
    releaser: Option<SemaphoreReleaser<'gate>>,
}

impl fmt::Debug for HardLimitGuard<'_> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HardLimitGuard")
            .field("active", &self.releaser.is_some())
            .finish()
    }
}

impl HardLimit {
    /// Yield to the runtime until a permit is available, then hand back
    /// an RAII guard. Disabled-mode returns immediately with a no-op
    /// guard.
    ///
    /// A notice is emitted to the sink (stdout in production) **only if
    /// the call actually had to wait** — never on the fast-path
    /// (`try_acquire` succeeds). The emitted line carries the measured
    /// wait duration.
    #[inline]
    pub async fn acquire(&self) -> HardLimitGuard<'_> {
        let Some(inner) = &self.inner else {
            return HardLimitGuard { releaser: None };
        };

        if let Some(releaser) = inner.sem.try_acquire(1) {
            return HardLimitGuard {
                releaser: Some(releaser),
            };
        }

        let parked_at = Instant::now();
        let releaser = inner.sem.acquire(1).await;
        let parked = parked_at.elapsed();

        (self.sink)(&format!(
            "rudzio: parked {parked:?} on parallel-hardlimit ({max} max); \
             disable with --threads-parallel-hardlimit=none",
            max = inner.max.get(),
        ));

        HardLimitGuard {
            releaser: Some(releaser),
        }
    }

    /// `None` = gate disabled, acquire is a no-op. `Some(n)` = at most
    /// `n` concurrent permits; additional acquirers `.await` until one
    /// is released.
    #[must_use]
    #[inline]
    pub fn new(limit: Option<NonZeroUsize>) -> Self {
        Self {
            inner: limit.map(Inner::with_max),
            sink: Box::new(|msg| write_stdout(&format!("{msg}\n"))),
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
    pub fn with_sink<S>(limit: Option<NonZeroUsize>, sink: S) -> Self
    where
        S: Fn(&str) + Send + Sync + 'static,
    {
        Self {
            inner: limit.map(Inner::with_max),
            sink: Box::new(sink),
        }
    }
}

impl Inner {
    /// Build an `Inner` with a fair (FIFO) semaphore initialised to
    /// `max` permits.
    #[inline]
    fn with_max(max: NonZeroUsize) -> Self {
        Self {
            max,
            sem: Semaphore::new(true, max.get()),
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

// Unit tests live as proper rudzio tests in
// `tests/parallelism_tests.rs`. The previous `#[cfg(test)] mod tests`
// here never ran — `[lib] test = false` in `Cargo.toml` disables the
// libtest target — and was invisible to `cargo rudzio test` for the
// same reason. The dedicated test crate dogfoods the framework and
// gives the suite a place to live.
