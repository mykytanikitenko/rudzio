//! Custom panic hook installed alongside stdio capture.
//!
//! Purpose, in order of importance:
//!
//! 1. **Restore FDs on a panic *outside* any test.** A panic in the
//!    runner itself (setup, drawer, the linkme discovery code, …)
//!    would otherwise write its backtrace through the captured
//!    stderr pipe into a drawer that is probably also unwinding, so
//!    the user would see a frozen terminal with no backtrace. The
//!    hook detects the "no current test" case via the thread-local
//!    [`CURRENT_TEST_ID`] and restores FDs 1 and 2 before chaining
//!    to the previous hook — so the backtrace lands on the real
//!    terminal.
//! 2. **Carry the previous panic hook behaviour for test panics**,
//!    which is just calling the previously-installed hook (usually
//!    the stdlib default). Test panics write via captured stderr as
//!    usual and the drawer renders them with the owning test's
//!    block.
//!
//! The thread-local state is populated by
//! [`crate::output::first_poll::FirstPoll`] on first poll and cleared
//! by the runtime thread when `TestCompleted` is about to be sent.

use std::cell::Cell;
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::events::TestId;
use super::pipe::SavedFds;

/// Process-wide count of panics observed by the rudzio panic hook
/// **outside** any per-test `catch_unwind` boundary.
///
/// Bumped by the hook for every panic where [`CURRENT_TEST_ID`] is
/// `None` — i.e., panics on background threads spawned by user code
/// (rustls crypto provider init, libc `pthread_create` callbacks, …)
/// that wouldn't otherwise show up in the per-test outcome stream.
///
/// The runner reads this counter at end-of-run. If it's non-zero AND
/// the regular test summary thinks the run is successful, the runner
/// flips the exit code to 1 and prints a warning. Otherwise the panics
/// are still visible (the default hook printed them to stderr) but
/// don't affect the exit code, since the captured failures already
/// account for them.
static UNATTRIBUTED_PANICS: AtomicUsize = AtomicUsize::new(0);

/// Read the unattributed-panic counter. Used by the runner's exit-code
/// path; tests can also call this to assert background-panic counting.
#[must_use]
pub fn unattributed_panic_count() -> usize {
    UNATTRIBUTED_PANICS.load(Ordering::Relaxed)
}

thread_local! {
    /// Set when the thread is currently executing a captured test's
    /// future (via [`crate::output::first_poll::FirstPoll`]). Read by
    /// the panic hook to decide whether a panic belongs to a test
    /// (let the default hook print through captured stderr) or to
    /// the runner (restore FDs first).
    static CURRENT_TEST_ID: Cell<Option<TestId>> = const { Cell::new(None) };
}

/// Mark the calling thread as running the given test. Pass `None` to
/// clear. Cheap — a thread-local `Cell::set`.
pub fn set_current_test(id: Option<TestId>) {
    CURRENT_TEST_ID.with(|c| c.set(id));
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install the custom panic hook once. Idempotent; subsequent calls
/// are no-ops. `saved_fds` is an `Arc` shared with the
/// [`crate::output::CaptureGuard`] so either side can restore
/// FDs (restore is internally idempotent — whichever runs first
/// takes ownership via atomic swap; the loser no-ops). Pass `None`
/// from plain-mode init — the counter still bumps but no FD restore
/// is needed because nothing was captured.
pub fn install(saved_fds: Option<Arc<SavedFds>>) {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let in_captured_test = CURRENT_TEST_ID.with(Cell::get).is_some();
        if !in_captured_test {
            // Panic outside a test — restore FDs so the backtrace lands
            // on the real terminal, AND bump the unattributed-panic
            // counter so the runner can flip the exit code if no test
            // outcome already accounts for the failure (e.g. a panic on
            // a thread spawned by user setup that doesn't unwind back
            // through our catch_unwind).
            if let Some(fds) = &saved_fds {
                fds.restore();
            }
            let _prev = UNATTRIBUTED_PANICS.fetch_add(1, Ordering::Relaxed);
        }
        prev(info);
    }));
}
