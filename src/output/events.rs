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

use crate::bench::ProgressSnapshot;
use crate::suite::{TeardownResult, TestOutcome};

/// Unique id for a test *dispatch*.
///
/// One per wrap in [`crate::output::first_poll::FirstPoll`]. Multiple
/// dispatches of the same test (e.g. re-runs, unlikely for rudzio but
/// possible in theory) get distinct ids. Monotonic within a single
/// process run; an `AtomicU64` counter hands them out in
/// [`TestId::next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub struct TestId(pub u64);

impl TestId {
    /// Construct a `TestId` from a raw counter value. Most callers
    /// should use [`Self::next`] instead; this exists so external
    /// crates can still build a `TestId` (e.g. for tests asserting on
    /// the wire format).
    #[inline]
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Allocate the next process-unique id. Wait-free.
    #[must_use]
    #[inline]
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Which standard stream a captured chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StdStream {
    /// FD 2 before capture.
    Stderr,
    /// FD 1 before capture.
    Stdout,
}

/// A raw byte chunk read from a captured pipe, not yet attributed.
///
/// The drawer attaches it to whichever test is currently mapped for
/// the thread that produced it, using its lifecycle-event-maintained
/// `thread_to_test` table.
#[derive(Debug)]
#[non_exhaustive]
pub struct PipeChunk {
    pub bytes: Vec<u8>,
    pub stream: StdStream,
}

impl PipeChunk {
    /// Construct a `PipeChunk` from its components.
    #[inline]
    #[must_use]
    pub const fn new(bytes: Vec<u8>, stream: StdStream) -> Self {
        Self { bytes, stream }
    }
}

/// A lifecycle event emitted by runtime threads (directly or via
/// `FirstPoll` / the panic hook) to the drawer.
#[derive(Debug)]
#[non_exhaustive]
pub enum LifecycleEvent {
    /// Periodic progress notification from a bench strategy.
    /// Strategies call the progress callback roughly every 1% of
    /// their iteration count; the drawer only renders the most
    /// recent value. The payload is `Copy` and allocation-free so it
    /// can travel through the lifecycle channel without overhead.
    BenchProgress {
        test_id: TestId,
        snapshot: ProgressSnapshot,
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
    /// `Suite::teardown` returned (possibly via panic).
    SuiteTeardownFinished {
        runtime_name: &'static str,
        suite: &'static str,
        thread: ThreadId,
        elapsed: Duration,
        result: TeardownResult,
    },
    /// A suite is about to run `Suite::teardown` (after all its tests
    /// have finished, regardless of outcome).
    SuiteTeardownStarted {
        runtime_name: &'static str,
        suite: &'static str,
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
    /// A per-test teardown (`Test::teardown`) returned `Err` or
    /// panicked. The drawer renders a separate FAIL line attributed
    /// to the test and pushes a `FailureRecord` so the final
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
#[non_exhaustive]
pub struct TestState {
    pub kind: TestStateKind,
    /// Most-recent `\n`-terminated line observed on either stream,
    /// kept around for clients that want a single-line summary; the
    /// live drawer renders the full output stream below the status
    /// row instead.
    pub last_output_line: String,
    pub module_path: &'static str,
    /// Every complete output line the test has emitted so far,
    /// oldest first. The live drawer streams these untruncated below
    /// the running status row, repainted in place every 50ms tick so
    /// the user sees stdout/stderr live as the test runs.
    pub recent_output: Vec<String>,
    pub runtime_name: &'static str,
    pub started_at: Instant,
    /// Captured stderr bytes, appended as chunks arrive.
    pub stderr_buffer: Vec<u8>,
    /// Captured stdout bytes, appended as chunks arrive.
    pub stdout_buffer: Vec<u8>,
    pub test_name: &'static str,
    pub thread: ThreadId,
}

/// Identity-and-thread fields of a [`TestState`] — bundled so
/// [`TestState::new`] takes one struct instead of five positional
/// args, sidestepping `clippy::too_many_arguments`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TestStateIdent {
    /// See [`TestState::module_path`].
    pub module_path: &'static str,
    /// See [`TestState::runtime_name`].
    pub runtime_name: &'static str,
    /// See [`TestState::started_at`].
    pub started_at: Instant,
    /// See [`TestState::test_name`].
    pub test_name: &'static str,
    /// See [`TestState::thread`].
    pub thread: ThreadId,
}

impl TestStateIdent {
    /// Pack the identity / thread / start-time fields.
    #[inline]
    #[must_use]
    pub const fn new(
        module_path: &'static str,
        runtime_name: &'static str,
        started_at: Instant,
        test_name: &'static str,
        thread: ThreadId,
    ) -> Self {
        Self { module_path, runtime_name, started_at, test_name, thread }
    }
}

/// Output-buffer fields of a [`TestState`] — bundled so
/// [`TestState::new`] takes one struct instead of four positional
/// args.
#[derive(Debug)]
#[non_exhaustive]
pub struct TestStateBuffers {
    /// See [`TestState::last_output_line`].
    pub last_output_line: String,
    /// See [`TestState::recent_output`].
    pub recent_output: Vec<String>,
    /// See [`TestState::stderr_buffer`].
    pub stderr_buffer: Vec<u8>,
    /// See [`TestState::stdout_buffer`].
    pub stdout_buffer: Vec<u8>,
}

impl TestStateBuffers {
    /// Empty buffers, suitable for a freshly-started test.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            last_output_line: String::new(),
            recent_output: Vec::new(),
            stderr_buffer: Vec::new(),
            stdout_buffer: Vec::new(),
        }
    }

    /// Pack the output-buffer fields.
    #[inline]
    #[must_use]
    pub const fn new(
        last_output_line: String,
        recent_output: Vec<String>,
        stderr_buffer: Vec<u8>,
        stdout_buffer: Vec<u8>,
    ) -> Self {
        Self { last_output_line, recent_output, stderr_buffer, stdout_buffer }
    }
}

impl TestState {
    /// Construct a `TestState` from its sub-bundles.
    #[inline]
    #[must_use]
    pub fn new(ident: TestStateIdent, buffers: TestStateBuffers, kind: TestStateKind) -> Self {
        let TestStateIdent { module_path, runtime_name, started_at, test_name, thread } = ident;
        let TestStateBuffers { last_output_line, recent_output, stderr_buffer, stdout_buffer } =
            buffers;
        Self {
            kind,
            last_output_line,
            module_path,
            recent_output,
            runtime_name,
            started_at,
            stderr_buffer,
            stdout_buffer,
            test_name,
            thread,
        }
    }
}

/// Current rendering state for a test's live-region slot.
///
/// `Bench`'s snapshot carries a 32-bucket histogram array, so it is
/// boxed to keep the unit `Running` variant cheap.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TestStateKind {
    /// Under a bench strategy; the most recent progress snapshot
    /// drives the trailing block + mini-histogram in the renderer.
    Bench {
        snapshot: Box<ProgressSnapshot>,
    },
    /// Ordinary test body running (no bench strategy).
    Running,
}
