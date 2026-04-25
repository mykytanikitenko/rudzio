//! Event types used by the capture + render pipeline.
//!
//! Runtime threads (and the [`crate::output::first_poll::FirstPoll`]
//! wrapper and the [`crate::output::panic_hook`]) produce
//! [`LifecycleEvent`]s; the pipe reader threads
//! ([`crate::output::reader`]) produce [`PipeChunk`]s. Both streams
//! flow into the drawer thread ([`crate::output::render`]) which
//! maintains the per-test [`TestState`] map and renders either a live
//! region (terminal mode) or linear text (plain mode).

use std::thread::ThreadId;
use std::time::{Duration, Instant};

use crate::suite::{TeardownResult, TestOutcome};

/// Unique id for a test *dispatch* — one per wrap in
/// [`crate::output::first_poll::FirstPoll`]. Multiple dispatches of the
/// same test (e.g. re-runs, unlikely for rudzio but possible in theory)
/// get distinct ids. Monotonic within a single process run; an
/// `AtomicU64` counter hands them out in [`TestId::next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TestId(pub u64);

impl TestId {
    /// Allocate the next process-unique id. Wait-free.
    #[must_use]
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Which standard stream a captured chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdStream {
    /// FD 1 before capture.
    Stdout,
    /// FD 2 before capture.
    Stderr,
}

/// A raw byte chunk read from a captured pipe, not yet attributed to
/// any test. The drawer attaches it to whichever test is currently
/// mapped for the thread that produced it, using its
/// lifecycle-event-maintained `thread_to_test` table.
#[derive(Debug)]
pub struct PipeChunk {
    pub stream: StdStream,
    pub bytes: Vec<u8>,
}

/// A lifecycle event emitted by runtime threads (directly or via
/// `FirstPoll` / the panic hook) to the drawer.
#[derive(Debug)]
pub enum LifecycleEvent {
    /// First `poll` of a test future — the moment the test is
    /// actually running, not merely scheduled. Establishes the
    /// `{thread → test_id}` mapping the drawer uses to attribute
    /// subsequent captured bytes.
    TestStarted {
        test_id: TestId,
        module_path: &'static str,
        test_name: &'static str,
        runtime_name: &'static str,
        thread: ThreadId,
        at: Instant,
    },
    /// Runtime thread has finished dispatching this test. Must be
    /// emitted *after* an explicit `io::stdout().flush()` +
    /// `io::stderr().flush()` so the drawer's drain-before-announce
    /// picks up the tail of the test's output. Elapsed time lives
    /// inside the [`TestOutcome`] variant.
    TestCompleted {
        test_id: TestId,
        outcome: TestOutcome,
    },
    /// A test was skipped because of `#[ignore]` and the current
    /// [`crate::config::RunIgnoredMode`]. Counts toward the
    /// "ignored" bucket of the final summary but does not go
    /// through the full TestStarted/TestCompleted dance.
    TestIgnored {
        module_path: &'static str,
        test_name: &'static str,
        runtime_name: &'static str,
        reason: &'static str,
    },
    /// Periodic progress notification from a bench strategy.
    /// Strategies call the progress callback roughly every 1% of
    /// their iteration count; the drawer only renders the most
    /// recent value.
    BenchProgress {
        test_id: TestId,
        done: usize,
        total: usize,
    },
    /// A suite is about to run `Suite::setup`. Emitted from the
    /// runtime group thread before the user's setup body executes.
    /// `thread` lets the drawer pin the in-flight setup to the same
    /// runtime slot that will host the suite's tests.
    SuiteSetupStarted {
        runtime_name: &'static str,
        suite: &'static str,
        thread: ThreadId,
        at: Instant,
    },
    /// `Suite::setup` returned. `error` is `None` on success and
    /// `Some(message)` on failure (the error's `Display` form).
    SuiteSetupFinished {
        runtime_name: &'static str,
        suite: &'static str,
        thread: ThreadId,
        elapsed: Duration,
        error: Option<String>,
    },
    /// A suite is about to run `Suite::teardown` (after all its tests
    /// have finished, regardless of outcome).
    SuiteTeardownStarted {
        runtime_name: &'static str,
        suite: &'static str,
        thread: ThreadId,
        at: Instant,
    },
    /// `Suite::teardown` returned (possibly via panic).
    SuiteTeardownFinished {
        runtime_name: &'static str,
        suite: &'static str,
        thread: ThreadId,
        elapsed: Duration,
        result: TeardownResult,
    },
    /// A per-test teardown (`Test::teardown`) returned `Err` or
    /// panicked. The drawer renders a separate FAIL line attributed
    /// to the test and pushes a FailureRecord so the final
    /// `failures:` section includes it.
    TestTeardownFailed {
        module_path: &'static str,
        test_name: &'static str,
        runtime_name: &'static str,
        result: TeardownResult,
    },
}

/// Drawer-owned state for a single in-flight test.
#[derive(Debug)]
pub struct TestState {
    pub module_path: &'static str,
    pub test_name: &'static str,
    pub runtime_name: &'static str,
    pub thread: ThreadId,
    pub started_at: Instant,
    pub kind: TestStateKind,
    /// Captured stdout bytes, appended as chunks arrive.
    pub stdout_buffer: Vec<u8>,
    /// Captured stderr bytes, appended as chunks arrive.
    pub stderr_buffer: Vec<u8>,
    /// Most-recent `\n`-terminated line observed on either stream,
    /// truncated to a printable width; displayed as the `↳` hint in
    /// the live region.
    pub last_output_line: String,
}

/// Current rendering state for a test's live-region slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStateKind {
    /// Ordinary test body running (no bench strategy).
    Running,
    /// Under a bench strategy; `done` of `total` iterations complete.
    Bench { done: usize, total: usize },
}
