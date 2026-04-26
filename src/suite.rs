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

use std::any::{Any, TypeId};
use std::marker::PhantomData;
use std::mem;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::time::Duration;

use futures_util::future::{AbortHandle, Aborted};
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
#[inline]
pub const fn fnv1a64(text: &str) -> u64 {
    let bytes = text.as_bytes();
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
    /// The test (body, per-test setup, or per-test teardown) blew its
    /// per-phase budget AND was still pending after `--phase-hang-grace`.
    /// The wrapper has fired its abort handle (cooperatively cancelling
    /// the spawned task on tokio) and moved on so subsequent tests can
    /// still run. Distinct from `TimedOut`: `[HANG]` (red) means the
    /// task ignored cooperative cancellation entirely; `[TIMEOUT]`
    /// (yellow) means it cancelled cleanly.
    Hung {
        elapsed: Duration,
    },
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
    /// The teardown blew its budget AND remained pending past the
    /// Layer-2 grace window. The driver fired its abort handle and
    /// moved on; on tokio the spawned task is gone, on other runtimes
    /// it leaks until process exit. Rendered as `[HANG teardown]`
    /// (red) — distinct from `[TIMEOUT teardown]` (yellow).
    Hung,
}

/// Extract a human-readable message from a `catch_unwind` panic
/// payload. Rust's panic payload is `Box<dyn Any + Send>`; the common
/// shapes are `&'static str` and `String`. Anything else is reported
/// as a generic placeholder so the user still sees that *something*
/// panicked rather than getting nothing.
#[must_use]
#[inline]
pub fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
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
    /// Number of tests escalated from `TimedOut` to `Hung` because
    /// they remained pending after `--phase-hang-grace`. Counted
    /// separately so the summary line can show `N hung` and the
    /// renderer can paint a distinct `[HANG]` tag.
    pub hung: usize,
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
            hung: 0,
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
            hung: self.hung.saturating_add(other.hung),
            ignored: self.ignored.saturating_add(other.ignored),
            total: self.total.saturating_add(other.total),
            teardown_failures: self
                .teardown_failures
                .saturating_add(other.teardown_failures),
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
    /// The phase blew its budget AND remained pending after
    /// `--phase-hang-grace`. The wrapper has fired its abort handle
    /// (where applicable) and stopped awaiting; on tokio the spawned
    /// task drops on next poll, on other runtimes it leaks until
    /// process exit. Surfaced through [`TestOutcome::Hung`] /
    /// [`TeardownResult::Hung`] downstream.
    Hung,
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
#[inline]
pub async fn run_phase_with_timeout_and_cancel<F, T, S>(
    phase_fut: F,
    phase_timeout: Option<Duration>,
    phase_hang_grace: Option<Duration>,
    phase_token: CancellationToken,
    sleep: impl Fn(Duration) -> S,
) -> PhaseOutcome<T>
where
    F: Future<Output = T>,
    S: Future<Output = ()>,
{
    use futures_util::FutureExt as _;
    use futures_util::future::{Either, select};

    let catch_fut = AssertUnwindSafe(phase_fut).catch_unwind();
    let cancellable = std::pin::pin!(phase_token.run_until_cancelled(catch_fut));

    let Some(dur) = phase_timeout else {
        return match cancellable.await {
            Some(Ok(value)) => PhaseOutcome::Completed(value),
            Some(Err(payload)) => PhaseOutcome::Panicked(panic_payload_message(&*payload)),
            None => PhaseOutcome::Cancelled,
        };
    };

    let sleep_fut = std::pin::pin!(sleep(dur));
    let still_pending = match select(cancellable, sleep_fut).await {
        Either::Left((Some(Ok(value)), _)) => return PhaseOutcome::Completed(value),
        Either::Left((Some(Err(payload)), _)) => {
            return PhaseOutcome::Panicked(panic_payload_message(&*payload));
        }
        Either::Left((None, _)) => return PhaseOutcome::Cancelled,
        Either::Right(((), still_pending)) => still_pending,
    };

    // Timeout fired. Cancel the phase token so cooperative phase
    // futures bail on next yield. Then enter the Layer-2 grace step.
    phase_token.cancel();

    let Some(grace) = phase_hang_grace else {
        // Layer-2 disabled: stop here with TimedOut (the phase future is
        // dropped with the surrounding `still_pending` going out of scope).
        return PhaseOutcome::TimedOut;
    };

    let grace_timer = std::pin::pin!(sleep(grace));
    match select(still_pending, grace_timer).await {
        // Phase completed cooperatively within the grace window —
        // either it returned, panicked, or honoured the cancel signal.
        // All three end as `TimedOut` (the budget WAS still blown);
        // `Hung` is reserved for the case where no progress was
        // observed at all.
        Either::Left(_) => PhaseOutcome::TimedOut,
        Either::Right(_) => PhaseOutcome::Hung,
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
#[inline]
pub async fn run_test_with_timeout_and_cancel<F, S>(
    test_fut: F,
    test_timeout: Option<Duration>,
    phase_hang_grace: Option<Duration>,
    per_test_token: CancellationToken,
    sleep: impl Fn(Duration) -> S,
) -> TestOutcome
where
    F: Future<Output = Result<(), BoxError>>,
    S: Future<Output = ()>,
{
    match run_phase_with_timeout_and_cancel(
        test_fut,
        test_timeout,
        phase_hang_grace,
        per_test_token,
        sleep,
    )
    .await
    {
        PhaseOutcome::Completed(Ok(())) => TestOutcome::Passed {
            elapsed: Duration::ZERO,
        },
        PhaseOutcome::Completed(Err(err)) => TestOutcome::Failed {
            elapsed: Duration::ZERO,
            message: err.to_string(),
        },
        PhaseOutcome::Panicked(_) => TestOutcome::Panicked {
            elapsed: Duration::ZERO,
        },
        PhaseOutcome::Cancelled => TestOutcome::Cancelled,
        PhaseOutcome::TimedOut => TestOutcome::TimedOut,
        PhaseOutcome::Hung => TestOutcome::Hung {
            elapsed: Duration::ZERO,
        },
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
        TestOutcome::Hung { .. } => TestOutcome::Hung { elapsed },
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
#[inline]
pub async fn run_bench_with_timeout_and_cancel<F, S>(
    bench_fut: F,
    test_timeout: Option<Duration>,
    phase_hang_grace: Option<Duration>,
    per_test_token: CancellationToken,
    sleep: impl Fn(Duration) -> S,
) -> TestOutcome
where
    F: Future<Output = BenchReport>,
    S: Future<Output = ()>,
{
    match run_phase_with_timeout_and_cancel(
        bench_fut,
        test_timeout,
        phase_hang_grace,
        per_test_token,
        sleep,
    )
    .await
    {
        PhaseOutcome::Completed(report) => TestOutcome::Benched {
            elapsed: Duration::ZERO,
            report,
        },
        PhaseOutcome::Panicked(_) => TestOutcome::Panicked {
            elapsed: Duration::ZERO,
        },
        PhaseOutcome::Cancelled => TestOutcome::Cancelled,
        PhaseOutcome::TimedOut => TestOutcome::TimedOut,
        PhaseOutcome::Hung => TestOutcome::Hung {
            elapsed: Duration::ZERO,
        },
    }
}

/// Drive a *spawned* phase future to completion or escalate it to
/// [`PhaseOutcome::Hung`] when it ignores cooperative cancellation.
///
/// `join_fut` is the awaitable returned by `Runtime::spawn` (or
/// equivalent), already wrapping the user's phase future in
/// [`futures_util::future::Abortable`]. `abort_handle` is the matching
/// abort half — calling it sets a flag the `Abortable` wrapper checks
/// at every poll, so on tokio the spawned task drops on its next
/// scheduler pass and the `join_fut` resolves promptly.
///
/// Why we need this distinct from
/// [`run_phase_with_timeout_and_cancel`]: the inline wrapper awaits
/// `phase_fut` on the *current* task. A sync-blocked phase (e.g.
/// `std::thread::sleep`) starves the wrapper's own poll loop, so the
/// timeout `select!` arm cannot fire — Layer-2's grace is meaningless.
/// `drive_per_test_spawn` runs on a separate task from the spawned
/// body, so the grace timer can always fire and we can move on.
///
/// Three-stage escalation:
///
/// 1. **Race phase** — race `join_fut` against `phase_token.cancelled()`
///    and `outer_budget`. Body completion → `Completed`. External
///    cancellation (parent) → fire `abort_handle`, return `Cancelled`.
///    Budget expiry → enter Stage 2.
/// 2. **Cooperative grace** — driver cancels `phase_token` (signalling
///    bodies that listen to it) and waits up to `outer_grace` for the
///    spawn to complete. If it does → `TimedOut`. If it doesn't →
///    Stage 3.
/// 3. **Forced abort** — fire `abort_handle` (sets the `Abortable`
///    flag, wakes the spawn's task, forcing the wrapper to return
///    `Aborted` on its next poll) and return `Hung`. The spawned
///    task is left to complete its drop on whatever schedule the
///    runtime decides.
///
/// Note that the driver itself **does not cancel `phase_token` on
/// parent cancellation** — `phase_token.cancelled()` already gives
/// us the signal, and re-cancelling would race with cooperative
/// listeners. The driver only cancels `phase_token` to communicate
/// "your budget is up" to bodies that opted into the token-listening
/// pattern.
#[doc(hidden)]
#[inline]
pub async fn drive_per_test_spawn<JoinFut, T, S>(
    join_fut: JoinFut,
    abort_handle: AbortHandle,
    outer_budget: Option<Duration>,
    outer_grace: Option<Duration>,
    phase_token: CancellationToken,
    sleep: impl Fn(Duration) -> S,
) -> PhaseOutcome<T>
where
    JoinFut: Future<Output = Result<T, Aborted>>,
    S: Future<Output = ()>,
{
    use futures_util::future::{Either, select};

    let mut join_fut = std::pin::pin!(join_fut);

    // Stage 1: race join, parent cancel, and budget.
    //
    // `futures_util::future::select` polls its first argument before
    // its second, so we put the cancel/budget side first to bias
    // toward "parent cancel wins ties with body completion". Without
    // the bias a body that returns immediately on parent cancel
    // (cooperative) would race the driver and sometimes show up as
    // `Completed` even though the parent pulled the plug.
    if let Some(budget) = outer_budget {
        // Race parent_cancel against budget_timer to know which
        // outer trigger fired.
        let parent_or_budget = async {
            let parent = std::pin::pin!(phase_token.cancelled());
            let timer = std::pin::pin!(sleep(budget));
            match select(parent, timer).await {
                Either::Left(_) => Stage1Trigger::Parent,
                Either::Right(_) => Stage1Trigger::Budget,
            }
        };
        let parent_or_budget = std::pin::pin!(parent_or_budget);
        match select(parent_or_budget, join_fut.as_mut()).await {
            Either::Left((Stage1Trigger::Parent, _)) => {
                // Run-level cancel (SIGINT / --run-timeout). Fire
                // abort so the spawned body doesn't outlive the
                // dispatch loop, then report Cancelled.
                abort_handle.abort();
                return PhaseOutcome::Cancelled;
            }
            Either::Left((Stage1Trigger::Budget, _)) => {
                // Fall through to Stage 2.
            }
            Either::Right((Ok(value), _)) => return PhaseOutcome::Completed(value),
            Either::Right((Err(_aborted), _)) => {
                // Inner phase wrapper inside the spawn fired Aborted
                // (e.g. its own per-phase timeout aborted its inner
                // future). The body responded, the spawn finished —
                // surface as TimedOut so the user sees "test exceeded
                // its budget" rather than a no-info cancellation.
                return PhaseOutcome::TimedOut;
            }
        }
    } else {
        // No outer budget: race parent_cancel against join.
        match select(std::pin::pin!(phase_token.cancelled()), join_fut.as_mut()).await {
            Either::Left(_) => {
                abort_handle.abort();
                return PhaseOutcome::Cancelled;
            }
            Either::Right((Ok(value), _)) => return PhaseOutcome::Completed(value),
            Either::Right((Err(_aborted), _)) => return PhaseOutcome::TimedOut,
        }
    }

    // Stage 2: budget fired. Signal cooperative cancel by cancelling
    // phase_token (bodies listening to it bail out on next yield),
    // then wait the grace window for the spawn to finish.
    phase_token.cancel();

    let Some(grace) = outer_grace else {
        // Layer-2 disabled: fire abort immediately so the leaked
        // spawn doesn't outlive us, return TimedOut.
        abort_handle.abort();
        return PhaseOutcome::TimedOut;
    };

    let grace_timer = std::pin::pin!(sleep(grace));
    match select(join_fut, grace_timer).await {
        // Body finished within the grace window — either Ok(value),
        // Aborted (inner phase wrapper aborted), or wall-cancelled
        // via the now-cancelled phase_token returned by an inner
        // wrapper. All three end as TimedOut: the budget WAS blown,
        // but the body cooperated.
        Either::Left((Ok(_) | Err(_), _)) => PhaseOutcome::TimedOut,
        // Stage 3: grace expired. Body did not respond to phase_token
        // cancellation. Fire abort_handle (Layer-3 forced kill —
        // takes effect on tokio for tasks that yield, leaks for
        // sync-blocked tasks until process::exit) and return Hung.
        Either::Right(_) => {
            abort_handle.abort();
            PhaseOutcome::Hung
        }
    }
}

/// Outcome of the parent-cancel-vs-budget inner race in
/// [`drive_per_test_spawn`]. Carried as a value so the outer race
/// can branch on which trigger fired without a second token check.
enum Stage1Trigger {
    Parent,
    Budget,
}

/// Helper for the macro's Layer-2/Layer-3 spawn pipeline: extends a
/// `Future + Send + 'a` into the `'static`-bound shape that
/// `Runtime::spawn` requires, by Box::pin'ing and transmuting the
/// trait object's lifetime.
///
/// # Safety
///
/// The transmute is sound only when the caller guarantees the future
/// is awaited (or its drop is otherwise observed) before any borrow
/// captured by the future expires. The macro arranges this by
/// awaiting the resulting handle through [`drive_per_test_spawn`]
/// before the per-test fn returns. On the Hung path the spawn handle
/// is dropped without being awaited; soundness then relies on the
/// runtime's `Drop` impl aborting non-blocking spawned tasks (which
/// drops their futures, releasing the captured borrows) BEFORE the
/// runtime memory itself is freed. Tokio's runtime drop satisfies
/// this for non-blocking tasks; sync-blocked tasks (e.g. via
/// `spawn_blocking`) that ignore their abort are caught by Layer-1's
/// `process::exit(2)` watchdog instead.
///
/// Using a free function here (rather than emitting the transmute
/// inline in the macro) sidesteps a known rustc HRTB limitation
/// (#100013) when the source type uses `'_` inside the macro's
/// HRTB-using emission.
#[doc(hidden)]
#[expect(unsafe_code, reason = "scoped spawn — see fn docstring")]
#[inline]
pub unsafe fn extend_phase_future_lifetime<'a, F, T>(
    fut: F,
) -> Pin<Box<dyn Future<Output = T> + Send + 'static>>
where
    F: Future<Output = T> + Send + 'a,
    T: Send + 'static,
{
    let pinned: Pin<Box<dyn Future<Output = T> + Send + 'a>> = Box::pin(fut);
    // SAFETY: see fn-level docstring — caller awaits or runtime-aborts
    // before any captured borrow expires.
    unsafe {
        mem::transmute::<
            Pin<Box<dyn Future<Output = T> + Send + 'a>>,
            Pin<Box<dyn Future<Output = T> + Send + 'static>>,
        >(pinned)
    }
}
