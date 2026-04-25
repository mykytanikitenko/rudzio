//! Per-`(runtime, suite)` orchestration abstraction.
//!
//! Each `#[rudzio::suite]` block emits one zero-sized type that implements
//! [`RuntimeGroupOwner`]. Multiple suite blocks declaring the same
//! `(runtime, suite)` pair are assigned the same
//! [`RuntimeGroupKey`] (a compile-time FNV-1a hash of the path strings) and
//! coalesced at startup so they share **one** OS thread, **one** runtime
//! instance, and **one** suite context. Within that single async loop,
//! per-test dispatch is performed via an HRTB unsafe fn pointer stored on
//! every [`TestToken`] — the owner provides its concrete runtime + suite
//! pointers, and the test fn casts them back to the matching concrete types
//! it was generated for. The `runtime_group_key` match makes this
//! safe-by-construction at the macro level.
//!
//! No `'static` substitution anywhere: the runtime and the suite live on
//! the owner's stack frame for the whole `run_group` call, and per-test
//! borrows are scoped to the HRTB fn's `'s` lifetime.

use std::any::TypeId;
use std::marker::PhantomData;
use std::pin::Pin;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::bench::BenchReport;
use crate::config::Config;
use crate::test_case::BoxError;
use crate::token::TestToken;

/// Compile-time FNV-1a-64 hash of `s`.
///
/// Used by the suite macro to derive a stable [`RuntimeGroupKey`] from the
/// concatenated `(runtime_path, suite_path)` token strings without needing
/// any runtime registry.
#[doc(hidden)]
#[must_use]
pub const fn fnv1a64(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// Stable identifier for a `(runtime_type, suite_type)` pair.
///
/// Two tokens with the same key share an OS thread, a runtime instance, and
/// a suite context, even if they were emitted by different
/// `#[rudzio::suite]` invocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeGroupKey(pub u64);

/// Legacy id retained for the public re-export surface. Now unused
/// internally but kept as a thin newtype around `TypeId` for callers that
/// reach for `rudzio::SuiteId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SuiteId(pub TypeId);

/// Per-test outcome reported back to the runner.
#[derive(Debug, Clone)]
pub enum TestOutcome {
    Passed {
        elapsed: Duration,
    },
    Failed {
        elapsed: Duration,
        message: String,
    },
    Panicked {
        elapsed: Duration,
    },
    /// The per-test context creation (`Suite::context`) returned `Err`
    /// before the test body could run. Counted as a failure for
    /// summary / exit-code purposes but rendered with a distinct
    /// `[SETUP]` status tag so the user sees that the test never ran.
    SetupFailed {
        elapsed: Duration,
        message: String,
    },
    TimedOut,
    Cancelled,
    /// The test ran under a [`crate::bench::Strategy`]. `report.is_success()`
    /// decides whether the overall outcome counts as passed or failed.
    Benched {
        elapsed: Duration,
        report: BenchReport,
    },
}

/// Result of a `Suite::teardown` (or per-test teardown) call. Used by
/// reporter lifecycle events so the drawer can distinguish a clean
/// teardown from a propagated error from a panic.
///
/// `Panicked` carries the panic payload's display form when one is
/// available (see [`panic_payload_message`]) so the user sees what
/// actually panicked, not just that something did.
#[derive(Debug, Clone)]
pub enum TeardownResult {
    Ok,
    Err(String),
    Panicked(String),
    /// The teardown future was still running when its per-phase
    /// timeout fired. Distinct from `Err` so the renderer can show
    /// `[TIMEOUT teardown]` instead of `[FAIL] teardown`.
    TimedOut,
}

/// Extract a human-readable message from a `catch_unwind` panic
/// payload. Rust's panic payload is `Box<dyn Any + Send>`; the common
/// shapes are `&'static str` and `String`. Anything else is reported
/// as a generic placeholder so the user still sees that *something*
/// panicked rather than getting nothing.
#[must_use]
pub fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with non-string payload".to_owned()
    }
}

/// Aggregated per-group counts.
#[derive(Debug, Clone, Copy, Default)]
pub struct SuiteSummary {
    pub passed: usize,
    pub failed: usize,
    pub panicked: usize,
    pub timed_out: usize,
    pub cancelled: usize,
    pub ignored: usize,
    pub total: usize,
    /// Number of suite-level teardown calls that returned `Err` or
    /// panicked. Bumped from the macro-generated dispatch after a
    /// teardown finishes; participates in [`is_success`](crate::runner::TestSummary::is_success)
    /// so a botched teardown fails the run even if every test passed.
    pub teardown_failures: usize,
}

impl SuiteSummary {
    #[inline]
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            passed: 0,
            failed: 0,
            panicked: 0,
            timed_out: 0,
            cancelled: 0,
            ignored: 0,
            total: 0,
            teardown_failures: 0,
        }
    }

    #[inline]
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        Self {
            passed: self.passed.saturating_add(other.passed),
            failed: self.failed.saturating_add(other.failed),
            panicked: self.panicked.saturating_add(other.panicked),
            timed_out: self.timed_out.saturating_add(other.timed_out),
            cancelled: self.cancelled.saturating_add(other.cancelled),
            ignored: self.ignored.saturating_add(other.ignored),
            total: self.total.saturating_add(other.total),
            teardown_failures: self.teardown_failures.saturating_add(other.teardown_failures),
        }
    }
}

/// Sink the [`RuntimeGroupOwner`] uses to publish per-test progress and
/// non-fatal warnings as soon as they happen, so the runner can render
/// `--format=pretty` lines and `--format=terse` dots in real time.
pub trait SuiteReporter: Send + Sync {
    /// A test was skipped because of `#[ignore]` and the current
    /// [`RunIgnoredMode`].
    fn report_ignored(&self, token: &'static TestToken, runtime_name: &'static str);
    /// A test was queued but never started because the run was cancelled
    /// mid-stream.
    fn report_cancelled(&self, token: &'static TestToken, runtime_name: &'static str);
    /// A test finished with the given outcome.
    fn report_outcome(
        &self,
        token: &'static TestToken,
        runtime_name: &'static str,
        outcome: TestOutcome,
    );
    /// Non-fatal diagnostic (runtime construction failure, etc.).
    fn report_warning(&self, message: &str);
    /// A suite is about to start `Suite::setup`. Paired with
    /// [`Self::report_suite_setup_finished`] so the drawer can show
    /// "setup in progress" and the user can see which suite is being
    /// initialised.
    fn report_suite_setup_started(&self, runtime_name: &'static str, suite: &'static str);
    /// `Suite::setup` returned. `error` is `None` on success and
    /// `Some(message)` on failure. `elapsed` is the wall-clock time
    /// the setup call took.
    fn report_suite_setup_finished(
        &self,
        runtime_name: &'static str,
        suite: &'static str,
        elapsed: Duration,
        error: Option<&str>,
    );
    /// A suite is about to start `Suite::teardown`. Paired with
    /// [`Self::report_suite_teardown_finished`].
    fn report_suite_teardown_started(&self, runtime_name: &'static str, suite: &'static str);
    /// `Suite::teardown` returned (possibly via panic).
    fn report_suite_teardown_finished(
        &self,
        runtime_name: &'static str,
        suite: &'static str,
        elapsed: Duration,
        result: TeardownResult,
    );
    /// A per-test teardown (`Test::teardown`) returned `Err` or
    /// panicked. The test's own outcome was already reported via
    /// [`Self::report_outcome`]; this method surfaces the cleanup
    /// failure as a separate, visible event and lets the runner bump
    /// its teardown-failure counter.
    fn report_test_teardown_failure(
        &self,
        token: &'static TestToken,
        runtime_name: &'static str,
        result: TeardownResult,
    );
}

/// Inputs handed to [`RuntimeGroupOwner::run_group`].
#[derive(Debug)]
pub struct SuiteRunRequest<'a> {
    pub tokens: &'a [&'static TestToken],
    /// Resolved CLI / environment configuration for this run. Shared by
    /// every group; runtime constructors may inspect it (e.g. to size
    /// worker pools).
    pub config: &'a Config,
    pub root_token: CancellationToken,
}

/// HRTB unsafe fn pointer stored on every [`TestToken`].
///
/// The owner picks `'s` (its local stack frame's lifetime), provides
/// `runtime_ptr` and `suite_ptr` cast from its own concrete `R`/`S` values,
/// and the macro-generated body casts them back. Safety relies on the
/// per-token `runtime_group_key` matching the owner's
/// [`RuntimeGroupOwner::group_key`] — guaranteed by the suite macro.
///
/// The returned future drives the test body end to end:
///   1. creates the per-test context via `Suite::context`;
///   2. dispatches the user's test fn (sync or async) under a per-test
///      cancellation token and the optional per-test timeout;
///   3. always runs `Test::teardown` (catching panics and surfacing them
///      via `reporter`).
pub type TestRunFn = for<'s> unsafe fn(
    runtime_ptr: *const (),
    suite_ptr: *const (),
    _phantom: PhantomData<&'s ()>,
    token: &'static TestToken,
    test_timeout: Option<Duration>,
    root_token: CancellationToken,
    reporter: &'s dyn SuiteReporter,
) -> Pin<Box<dyn Future<Output = TestOutcome> + 's>>;

/// Per-`(runtime_type, suite_type)` lifecycle owner.
///
/// The macro emits one ZST per `#[rudzio::suite]` invocation; instances with
/// the same [`group_key`] are functionally equivalent and the runner picks
/// any one of them to drive a group. The runtime's display label comes from
/// [`Runtime::name`](crate::runtime::Runtime::name); the owner itself
/// carries only the stable group id.
pub trait RuntimeGroupOwner: Send + Sync + 'static {
    /// Stable id derived from the `(runtime_path, suite_path)` token
    /// strings at macro-time.
    fn group_key(&self) -> RuntimeGroupKey;

    /// Drive the whole group: create runtime, set up suite, dispatch every
    /// `req.tokens` entry via its [`TestToken::run_test`] fn pointer, tear
    /// down. Called from a dedicated OS thread.
    fn run_group(&self, req: SuiteRunRequest<'_>, reporter: &dyn SuiteReporter) -> SuiteSummary;
}

/// Outcome of a phase wrapped by [`run_phase_with_timeout_and_cancel`].
///
/// One variant per terminal state the wrapper distinguishes: a clean
/// completion (carries the phase future's own return value), a panic
/// caught via `catch_unwind` (carries the formatted payload), an
/// external cancellation propagated from the parent token, or a
/// per-phase timeout.
///
/// `Cancelled` and `TimedOut` are deliberately split: the former means
/// "someone above us pulled the plug" (run-timeout, ctrl+c, sibling
/// failure) and the latter means "this phase blew its own budget".
/// Reporters use the distinction to render the right status tag.
#[derive(Debug)]
pub enum PhaseOutcome<T> {
    Completed(T),
    Panicked(String),
    Cancelled,
    TimedOut,
}

/// Race a phase future against an optional per-phase timeout, while
/// staying responsive to a parent cancellation token.
///
/// This is the canonical primitive every phase (suite setup, suite
/// teardown, per-test setup, test body, per-test teardown) wraps
/// itself in. The pattern reuses [`tokio_util::sync::CancellationToken::run_until_cancelled`]
/// for the cancellation half and [`futures_util::future::select`]
/// against a caller-supplied sleep fn for the timeout half.
///
/// On `TimedOut`, the wrapper cancels `phase_token` so a cooperative
/// phase future (one that polls the token in tight loops) can bail out
/// immediately on its next yield.
///
/// `sleep` is supplied by the caller so each runtime can hand in its
/// own timer (`tokio::time::sleep`, `compio::time::sleep`, …) without
/// the wrapper depending on any one runtime crate.
#[doc(hidden)]
pub async fn run_phase_with_timeout_and_cancel<F, T, S>(
    phase_fut: F,
    phase_timeout: Option<Duration>,
    phase_token: CancellationToken,
    sleep: impl FnOnce(Duration) -> S,
) -> PhaseOutcome<T>
where
    F: Future<Output = T>,
    S: Future<Output = ()>,
{
    use futures_util::FutureExt as _;
    use futures_util::future::{Either, select};

    let catch_fut = std::panic::AssertUnwindSafe(phase_fut).catch_unwind();
    let cancellable = std::pin::pin!(phase_token.run_until_cancelled(catch_fut));

    if let Some(dur) = phase_timeout {
        let sleep_fut = std::pin::pin!(sleep(dur));
        match select(cancellable, sleep_fut).await {
            Either::Left((Some(Ok(value)), _)) => PhaseOutcome::Completed(value),
            Either::Left((Some(Err(payload)), _)) => {
                PhaseOutcome::Panicked(panic_payload_message(&*payload))
            }
            Either::Left((None, _)) => PhaseOutcome::Cancelled,
            Either::Right(_) => {
                phase_token.cancel();
                PhaseOutcome::TimedOut
            }
        }
    } else {
        match cancellable.await {
            Some(Ok(value)) => PhaseOutcome::Completed(value),
            Some(Err(payload)) => PhaseOutcome::Panicked(panic_payload_message(&*payload)),
            None => PhaseOutcome::Cancelled,
        }
    }
}

/// Runs `test_fut` under the per-test cancellation token and the optional
/// per-test timeout, classifying the resulting state into a [`TestOutcome`].
///
/// Thin caller of [`run_phase_with_timeout_and_cancel`]; the wrapper
/// provides timeout + cancel + panic catching, and this fn maps the
/// result into the test-body-shaped `TestOutcome` variants.
///
/// The `elapsed` field is left at `Duration::ZERO`; the caller fills it in.
///
/// Used by macro-generated per-test fns. No `Send` bound on
/// `test_fut`/`sleep` — the owner drives them inside `block_on` on the
/// calling thread, never spawned, so single-threaded runtimes (and `!Send`
/// test bodies on them) work too.
#[doc(hidden)]
pub async fn run_test_with_timeout_and_cancel<F, S>(
    test_fut: F,
    test_timeout: Option<Duration>,
    per_test_token: CancellationToken,
    sleep: impl FnOnce(Duration) -> S,
) -> TestOutcome
where
    F: Future<Output = Result<(), BoxError>>,
    S: Future<Output = ()>,
{
    match run_phase_with_timeout_and_cancel(test_fut, test_timeout, per_test_token, sleep).await {
        PhaseOutcome::Completed(Ok(())) => TestOutcome::Passed {
            elapsed: Duration::ZERO,
        },
        PhaseOutcome::Completed(Err(e)) => TestOutcome::Failed {
            elapsed: Duration::ZERO,
            message: e.to_string(),
        },
        PhaseOutcome::Panicked(_) => TestOutcome::Panicked {
            elapsed: Duration::ZERO,
        },
        PhaseOutcome::Cancelled => TestOutcome::Cancelled,
        PhaseOutcome::TimedOut => TestOutcome::TimedOut,
    }
}

/// Apply an `Instant`-based elapsed value to the outcomes that carry one.
#[doc(hidden)]
#[inline]
#[must_use]
pub fn fill_elapsed(outcome: TestOutcome, elapsed: Duration) -> TestOutcome {
    match outcome {
        TestOutcome::Passed { .. } => TestOutcome::Passed { elapsed },
        TestOutcome::Failed { message, .. } => TestOutcome::Failed { elapsed, message },
        TestOutcome::Panicked { .. } => TestOutcome::Panicked { elapsed },
        TestOutcome::Benched { report, .. } => TestOutcome::Benched { elapsed, report },
        other => other,
    }
}

/// Drive a benchmark future under the per-test cancellation token and the
/// optional per-test timeout, wrapping the resulting [`BenchReport`] into
/// a [`TestOutcome::Benched`] (or one of the terminal variants if the
/// bench timed out, was cancelled, or panicked).
///
/// Mirrors [`run_test_with_timeout_and_cancel`] but for the bench path —
/// the inner future yields a `BenchReport` rather than a
/// `Result<(), BoxError>`, so the classification logic is subtly
/// different (there's no "failed" variant here: per-iteration failures
/// are already captured inside the report).
#[doc(hidden)]
pub async fn run_bench_with_timeout_and_cancel<F, S>(
    bench_fut: F,
    test_timeout: Option<Duration>,
    per_test_token: CancellationToken,
    sleep: impl FnOnce(Duration) -> S,
) -> TestOutcome
where
    F: Future<Output = BenchReport>,
    S: Future<Output = ()>,
{
    match run_phase_with_timeout_and_cancel(bench_fut, test_timeout, per_test_token, sleep).await {
        PhaseOutcome::Completed(report) => TestOutcome::Benched {
            elapsed: Duration::ZERO,
            report,
        },
        PhaseOutcome::Panicked(_) => TestOutcome::Panicked {
            elapsed: Duration::ZERO,
        },
        PhaseOutcome::Cancelled => TestOutcome::Cancelled,
        PhaseOutcome::TimedOut => TestOutcome::TimedOut,
    }
}
