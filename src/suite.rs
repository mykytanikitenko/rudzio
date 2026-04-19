//! Per-suite runner abstraction.
//!
//! Each `#[rudzio::suite]` invocation generates one zero-sized type that
//! implements [`SuiteRunner`]. The implementation owns the orchestration of
//! its suite end-to-end: it creates the concrete runtime, sets up the global
//! context, dispatches each selected test against the right concrete test
//! function, and tears everything down — all inside a single function with
//! locally scoped lifetimes.
//!
//! The runner module groups [`TestToken`]s by [`SuiteId`] and hands the
//! per-suite slice to the matching `SuiteRunner`, which is the same `&'static
//! dyn SuiteRunner` for every token belonging to that suite.

use std::any::TypeId;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::test_case::BoxError;
use crate::token::TestToken;

/// Stable identifier for a `#[rudzio::suite]` instance.
///
/// Wraps the `TypeId` of a macro-generated zero-sized marker struct that
/// carries no lifetime parameters of its own — keeping the chain
/// `'runtime: 'global_context: 'test_context` from leaking into key material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SuiteId(pub TypeId);

/// How `#[ignore]`d tests should be treated for this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunIgnoredMode {
    /// Default: skip tests marked `#[ignore]`, report them as ignored.
    Normal,
    /// `--ignored`: only run ignored tests.
    Only,
    /// `--include-ignored`: run every test, ignored or not.
    Include,
}

/// Per-test outcome reported back to the runner.
#[derive(Debug)]
pub enum TestOutcome {
    Passed { elapsed: Duration },
    Failed { elapsed: Duration, message: String },
    Panicked { elapsed: Duration },
    TimedOut,
    Cancelled,
}

/// Aggregated per-suite counts.
#[derive(Debug, Clone, Copy, Default)]
pub struct SuiteSummary {
    pub passed: usize,
    pub failed: usize,
    pub panicked: usize,
    pub timed_out: usize,
    pub cancelled: usize,
    pub ignored: usize,
    pub total: usize,
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
        }
    }
}

/// Sink the [`SuiteRunner`] uses to publish per-test progress and
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
    /// Non-fatal diagnostic (teardown failures, etc.).
    fn report_warning(&self, message: &str);
}

/// Inputs handed to [`SuiteRunner::run_suite`].
///
/// Borrows `tokens` from the main runner — the suite must not retain it
/// past the call.
#[derive(Debug)]
pub struct SuiteRunRequest<'a> {
    pub tokens: &'a [&'static TestToken],
    pub threads: usize,
    pub test_timeout: Option<Duration>,
    pub run_ignored: RunIgnoredMode,
    pub root_token: CancellationToken,
}

/// Per-suite orchestration trait.
///
/// Implemented by macro-generated ZSTs. The implementation creates the
/// runtime as a local value, lets the global context borrow it, hands the
/// per-test context out by reference, and tears everything down — all
/// without `'static` substitution and without needing `Any` for downcasting.
pub trait SuiteRunner: Send + Sync + 'static {
    /// Identifier shared by every [`TestToken`] belonging to this suite.
    fn suite_id(&self) -> SuiteId;

    /// Display name of the runtime constructor (e.g. `"Multithread::new"`).
    fn runtime_name(&self) -> &'static str;

    /// Drive the suite to completion on the calling OS thread.
    fn run_suite(
        &self,
        req: SuiteRunRequest<'_>,
        reporter: &dyn SuiteReporter,
    ) -> SuiteSummary;
}

/// Runs `test_fut` under the per-test cancellation token and the optional
/// per-test timeout, classifying the resulting state into a [`TestOutcome`].
///
/// The `elapsed` field on the resulting outcome is left at `Duration::ZERO`;
/// the caller (which knows the start `Instant`) is expected to fill it in.
///
/// Used by macro-generated suite implementations. No `Send` bound on
/// `test_fut`/`sleep` — the suite runner drives them inside `block_on` on
/// the calling thread, never spawned, so single-threaded runtimes (and
/// `!Send` test bodies on them) work too.
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
    use futures_util::FutureExt as _;
    use futures_util::future::{Either, select};

    let catch_fut = std::panic::AssertUnwindSafe(test_fut).catch_unwind();
    let cancellable = std::pin::pin!(per_test_token.run_until_cancelled(catch_fut));

    if let Some(dur) = test_timeout {
        let sleep_fut = std::pin::pin!(sleep(dur));
        match select(cancellable, sleep_fut).await {
            Either::Left((Some(Ok(Ok(()))), _)) => {
                TestOutcome::Passed { elapsed: Duration::ZERO }
            }
            Either::Left((Some(Ok(Err(e))), _)) => TestOutcome::Failed {
                elapsed: Duration::ZERO,
                message: e.to_string(),
            },
            Either::Left((Some(Err(_payload)), _)) => {
                TestOutcome::Panicked { elapsed: Duration::ZERO }
            }
            Either::Left((None, _)) => TestOutcome::Cancelled,
            Either::Right(_) => {
                per_test_token.cancel();
                TestOutcome::TimedOut
            }
        }
    } else {
        match cancellable.await {
            Some(Ok(Ok(()))) => TestOutcome::Passed { elapsed: Duration::ZERO },
            Some(Ok(Err(e))) => TestOutcome::Failed {
                elapsed: Duration::ZERO,
                message: e.to_string(),
            },
            Some(Err(_payload)) => TestOutcome::Panicked { elapsed: Duration::ZERO },
            None => TestOutcome::Cancelled,
        }
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
        other => other,
    }
}
