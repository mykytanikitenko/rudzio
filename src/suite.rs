//! Per-`(runtime, global)` orchestration abstraction.
//!
//! Each `#[rudzio::suite]` block emits one zero-sized type that implements
//! [`RuntimeGroupOwner`]. Multiple suite blocks declaring the same
//! `(runtime, global_context)` pair are assigned the same
//! [`RuntimeGroupKey`] (a compile-time FNV-1a hash of the path strings) and
//! coalesced at startup so they share **one** OS thread, **one** runtime
//! instance, and **one** global context. Within that single async loop,
//! per-test dispatch is performed via an HRTB unsafe fn pointer stored on
//! every [`TestToken`] — the owner provides its concrete runtime + global
//! pointers, and the test fn casts them back to the matching concrete types
//! it was generated for. The `runtime_group_key` match makes this
//! safe-by-construction at the macro level.
//!
//! No `'static` substitution anywhere: the runtime and the global live on
//! the owner's stack frame for the whole `run_group` call, and per-test
//! borrows are scoped to the HRTB fn's `'g` lifetime.

use std::any::TypeId;
use std::marker::PhantomData;
use std::pin::Pin;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::test_case::BoxError;
use crate::token::TestToken;

/// Compile-time FNV-1a-64 hash of `s`.
///
/// Used by the suite macro to derive a stable [`RuntimeGroupKey`] from the
/// concatenated `(runtime_path, global_path)` token strings without needing
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

/// Stable identifier for a `(runtime_type, global_type)` pair.
///
/// Two tokens with the same key share an OS thread, a runtime instance, and
/// a global context, even if they were emitted by different
/// `#[rudzio::suite]` invocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeGroupKey(pub u64);

/// Legacy id retained for the public re-export surface. Now unused
/// internally but kept as a thin newtype around `TypeId` for callers that
/// reach for `rudzio::SuiteId`.
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
    /// Non-fatal diagnostic (teardown failures, etc.).
    fn report_warning(&self, message: &str);
}

/// Inputs handed to [`RuntimeGroupOwner::run_group`].
#[derive(Debug)]
pub struct SuiteRunRequest<'a> {
    pub tokens: &'a [&'static TestToken],
    pub threads: usize,
    pub test_timeout: Option<Duration>,
    pub run_ignored: RunIgnoredMode,
    pub root_token: CancellationToken,
}

/// HRTB unsafe fn pointer stored on every [`TestToken`].
///
/// The owner picks `'g` (its local stack frame's lifetime), provides
/// `runtime_ptr` and `global_ptr` cast from its own concrete `R`/`G` values,
/// and the macro-generated body casts them back. Safety relies on the
/// per-token `runtime_group_key` matching the owner's
/// [`RuntimeGroupOwner::group_key`] — guaranteed by the suite macro.
///
/// The returned future drives the test body end to end:
///   1. creates the per-test context via `Global::context`;
///   2. dispatches the user's test fn (sync or async) under a per-test
///      cancellation token and the optional per-test timeout;
///   3. always runs `Test::teardown` (catching panics and surfacing them
///      via `reporter`).
pub type TestRunFn = for<'g> unsafe fn(
    runtime_ptr: *const (),
    global_ptr: *const (),
    _phantom: PhantomData<&'g ()>,
    token: &'static TestToken,
    test_timeout: Option<Duration>,
    root_token: CancellationToken,
    reporter: &'g dyn SuiteReporter,
) -> Pin<Box<dyn Future<Output = TestOutcome> + 'g>>;

/// Per-`(runtime_type, global_type)` lifecycle owner.
///
/// The macro emits one ZST per `#[rudzio::suite]` invocation; instances with
/// the same [`group_key`] are functionally equivalent and the runner picks
/// any one of them to drive a group.
pub trait RuntimeGroupOwner: Send + Sync + 'static {
    /// Stable id derived from the `(runtime_path, global_path)` token
    /// strings at macro-time.
    fn group_key(&self) -> RuntimeGroupKey;

    /// Display name of the runtime constructor (e.g. `"Multithread::new"`).
    fn runtime_name(&self) -> &'static str;

    /// Drive the whole group: create runtime, set up global, dispatch every
    /// `req.tokens` entry via its [`TestToken::run_test`] fn pointer, tear
    /// down. Called from a dedicated OS thread.
    fn run_group(
        &self,
        req: SuiteRunRequest<'_>,
        reporter: &dyn SuiteReporter,
    ) -> SuiteSummary;
}

/// Runs `test_fut` under the per-test cancellation token and the optional
/// per-test timeout, classifying the resulting state into a [`TestOutcome`].
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
