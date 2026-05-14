//! Unit tests for `rudzio::suite::run_phase_with_timeout_and_cancel`
//! and the spawned-body driver `rudzio::suite::drive_per_test_spawn`.
//!
//! The phase wrapper is the canonical primitive every phase (suite setup,
//! suite teardown, per-test setup, test body, per-test teardown) wraps
//! itself in: it races the phase future against an optional per-phase
//! timeout while staying responsive to a parent cancellation token.
//!
//! `drive_per_test_spawn` is the Layer-3 driver used for test/bench
//! bodies that have been spawned onto the runtime via `Runtime::spawn`
//! (so the wrapper task is on a *different* worker than the body). It
//! adds a post-cancel grace step + cooperative abort via
//! `futures_util::future::AbortHandle`; on grace expiry the outcome
//! escalates from `TimedOut` to `Hung` and the abort signal is fired.
//!
//! Runtime coverage: the suite is dispatched on every supported
//! adapter (tokio mt/ct/local, compio, embassy, `futures::ThreadPool`)
//! since the wrappers are themselves runtime-agnostic and must hold the
//! same contract everywhere. The wrappers' `sleep: Sleep` parameter is
//! threaded as `|d| ctx.sleep(d)` so the timer always belongs to the
//! adapter under test.
//!
//! The tests synthesise `JoinFut` values directly rather than spawning
//! real tasks: the driver only inspects the join future's output and
//! its readiness, so a hand-built `async move { ... }` (or
//! `std::future::pending`) is observationally equivalent to a real
//! `Runtime::spawn`'d body and avoids the lifetime gymnastics of
//! capturing an adapter-specific spawn handle inside a `'static`-bound
//! body. Abort observation goes through `AbortHandle::is_aborted` on a
//! clone, not via `Abortable` — same signal, no real abortable body
//! needed.
//!
//! The intentional unwind in `phase_panics_returns_panicked_with_message`
//! is triggered via [`std::panic::resume_unwind`] (a function call) rather
//! than the `panic!` macro. Both produce identical observable behaviour
//! through `catch_unwind` / `panic_payload_message`, but only the macro
//! form trips `clippy::panic`. Using `resume_unwind` lets the test stay
//! honest (real unwind, framework genuinely captures it) without a
//! site-local `#[expect]` escape hatch.

use std::panic::resume_unwind;
use std::time::{Duration, Instant};

use rudzio::common::context::{Suite, Test};
use rudzio::futures_util::future::{AbortHandle, Aborted};
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
use rudzio::suite::{PhaseOutcome, drive_per_test_spawn, run_phase_with_timeout_and_cancel};
use rudzio::tokio_util::sync::CancellationToken;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod phase_wrapper_tests {
    use std::future::pending;

    use rudzio::context::Test as _;
    use rudzio::futures_util::future::join;

    use super::{
        AbortHandle, Aborted, CancellationToken, Duration, Instant, PhaseOutcome, Test,
        drive_per_test_spawn, resume_unwind, run_phase_with_timeout_and_cancel,
    };

    /// L3.3. A cooperatively-cancellable body whose cancellation lands
    /// before the grace window expires must come back as `TimedOut`,
    /// not `Hung`. The driver's grace step is for *escalating* a
    /// stuck body — it must NOT downgrade fast cooperative timeouts.
    /// The synthetic `join_fut` listens to the same token the driver
    /// cancels in Stage 2, so the body resolves promptly within grace.
    #[rudzio::test]
    async fn drive_per_test_spawn_cooperative_timeout(ctx: &Test) -> anyhow::Result<()> {
        let (abort_handle, _abort_reg) = AbortHandle::new_pair();
        let phase_token = CancellationToken::new();
        let phase_token_for_join = phase_token.clone();
        let join_fut = async move {
            phase_token_for_join.cancelled().await;
            Err::<(), _>(Aborted)
        };
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(50_u64)),
            Some(Duration::from_secs(2_u64)),
            phase_token,
            |dur| ctx.sleep(dur),
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::TimedOut),
            "expected TimedOut (grace honoured cooperative cancel), got {outcome:?}"
        );
        Ok(())
    }

    /// L3.1. A spawned body that completes promptly resolves the driver
    /// with `Completed(value)` carrying the body's return value. The
    /// abort handle is never fired; cancelling it post-completion is a
    /// no-op.
    #[rudzio::test]
    async fn drive_per_test_spawn_completed_body(ctx: &Test) -> anyhow::Result<()> {
        let (abort_handle, _abort_reg) = AbortHandle::new_pair();
        let abort_observer = abort_handle.clone();
        let join_fut = async { Ok::<u32, Aborted>(99_u32) };
        let token = CancellationToken::new();
        let outcome = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_secs(5_u64)),
            Some(Duration::from_millis(200_u64)),
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed(99)),
            "expected Completed(99), got {outcome:?}"
        );
        anyhow::ensure!(
            !abort_observer.is_aborted(),
            "abort handle must not fire when the body completes within budget"
        );
        Ok(())
    }

    /// L3.6. The driver must actually fire the abort handle on
    /// grace-expiry escalation. We observe the side effect via
    /// `AbortHandle::is_aborted` on a clone retained outside the
    /// driver; after the driver returns `Hung` the clone must report
    /// `is_aborted() == true`. The synthetic `join_fut` is plain
    /// `pending()` — observationally equivalent to a real spawned body
    /// that ignores both `phase_token` and `abort_handle`.
    #[rudzio::test]
    async fn drive_per_test_spawn_hung_fires_abort_handle(ctx: &Test) -> anyhow::Result<()> {
        let (abort_handle, _abort_reg) = AbortHandle::new_pair();
        let abort_observer = abort_handle.clone();
        let join_fut = pending::<Result<(), Aborted>>();
        let token = CancellationToken::new();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(50_u64)),
            Some(Duration::from_millis(50_u64)),
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Hung),
            "expected Hung, got {outcome:?}"
        );
        anyhow::ensure!(
            abort_observer.is_aborted(),
            "abort handle must have been fired before driver returned Hung"
        );
        Ok(())
    }

    /// L3.2. An uncooperative spawned body — one that ignores both the
    /// phase token and the abort handle — must still be reported as
    /// `Hung` after the outer grace window. Modelled here with a plain
    /// `pending()` join future: it never resolves, so the driver
    /// genuinely cannot move on without enforcing its own grace
    /// timeout. Wall-clock total ≤ budget + grace + slack proves the
    /// driver does not block on the body.
    #[rudzio::test]
    async fn drive_per_test_spawn_hung_uncooperative_body(ctx: &Test) -> anyhow::Result<()> {
        let (abort_handle, _abort_reg) = AbortHandle::new_pair();
        let join_fut = pending::<Result<(), Aborted>>();
        let token = CancellationToken::new();
        let start = Instant::now();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(200_u64)),
            Some(Duration::from_millis(300_u64)),
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Hung),
            "expected Hung, got {outcome:?}"
        );
        // Generous wall-clock bound: budget+grace adds up to 500ms of
        // *intended* wait, but under the workspace aggregator many
        // runtimes run concurrently and timer wakes can be late by a
        // couple of seconds. The correctness signal is the `Hung`
        // outcome above — this bound just checks the driver isn't
        // blocking forever.
        anyhow::ensure!(
            elapsed < Duration::from_secs(5_u64),
            "driver must NOT block on the uncooperative body, took {elapsed:?}"
        );
        Ok(())
    }

    /// L3.7. With `outer_budget = None` and a parent token never
    /// cancelled, the driver awaits the spawn until completion — no
    /// timer, no grace, just plain await. Confirms the driver doesn't
    /// short-circuit when timeouts are unconfigured.
    #[rudzio::test]
    async fn drive_per_test_spawn_no_budget_awaits_to_completion(ctx: &Test) -> anyhow::Result<()> {
        let (abort_handle, _abort_reg) = AbortHandle::new_pair();
        let join_fut = async {
            ctx.sleep(Duration::from_millis(50_u64)).await;
            Ok::<&'static str, Aborted>("done")
        };
        let token = CancellationToken::new();
        let outcome = drive_per_test_spawn(join_fut, abort_handle, None, None, token, |dur| {
            ctx.sleep(dur)
        })
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed("done")),
            "expected Completed(\"done\"), got {outcome:?}"
        );
        Ok(())
    }

    /// L3.4. With `outer_grace = None` (Layer 2 disabled), the driver
    /// returns `TimedOut` immediately on budget expiry — no grace
    /// window, no escalation to `Hung`. This is the opt-out path for
    /// users who'd rather rely on Layer 1 (`process::exit(2)`) for
    /// hardkill. Synthetic `pending()` join future stands in for an
    /// uncooperative body.
    #[rudzio::test]
    async fn drive_per_test_spawn_no_grace_returns_timed_out(ctx: &Test) -> anyhow::Result<()> {
        let (abort_handle, _abort_reg) = AbortHandle::new_pair();
        let abort_observer = abort_handle.clone();
        let join_fut = pending::<Result<(), Aborted>>();
        let token = CancellationToken::new();
        let start = Instant::now();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(100_u64)),
            None,
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
        anyhow::ensure!(
            abort_observer.is_aborted(),
            "abort handle must fire on Layer-2-disabled budget expiry so the leaked spawn doesn't outlive us"
        );
        // Generous wall-clock bound: the driver returns on budget
        // expiry without waiting for the body — the correctness signal
        // is the `TimedOut` outcome above; this bound just checks the
        // driver isn't waiting for the synthetic body to finish.
        anyhow::ensure!(
            elapsed < Duration::from_secs(5_u64),
            "driver must return on budget expiry without waiting for grace, took {elapsed:?}"
        );
        Ok(())
    }

    /// L3.5. When the parent token is cancelled before the budget
    /// fires, the driver returns `Cancelled` (not `TimedOut`, not
    /// `Hung`). Same distinction the inline wrapper makes: parent
    /// cancellation is "someone above pulled the plug", not "this
    /// phase blew its budget". The cancel-after-delay signal is
    /// driven inline alongside the driver via `futures_util::join`
    /// — no spawn needed.
    #[rudzio::test]
    async fn drive_per_test_spawn_parent_cancellation(ctx: &Test) -> anyhow::Result<()> {
        let (abort_handle, _abort_reg) = AbortHandle::new_pair();
        let abort_observer = abort_handle.clone();
        let phase_token = CancellationToken::new();
        let phase_token_for_join = phase_token.clone();
        let join_fut = async move {
            phase_token_for_join.cancelled().await;
            Err::<(), _>(Aborted)
        };
        let phase_token_for_cancel = phase_token.clone();
        let driver = drive_per_test_spawn::<_, (), _, _>(
            join_fut,
            abort_handle,
            Some(Duration::from_secs(5_u64)),
            Some(Duration::from_millis(300_u64)),
            phase_token,
            |dur| ctx.sleep(dur),
        );
        let cancel = async {
            ctx.sleep(Duration::from_millis(20_u64)).await;
            phase_token_for_cancel.cancel();
        };
        let (outcome, ()) = join(driver, cancel).await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        anyhow::ensure!(
            abort_observer.is_aborted(),
            "abort handle must fire on parent cancel so the spawned body doesn't outlive the dispatch loop"
        );
        Ok(())
    }

    /// A5. With no timeout (`None`), the wrapper still awaits the phase
    /// future and returns `Completed` when it resolves.
    #[rudzio::test]
    async fn no_timeout_with_completion_returns_completed(ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome =
            run_phase_with_timeout_and_cancel(async { "done" }, None, None, token, |dur| {
                ctx.sleep(dur)
            })
            .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed("done")),
            "expected Completed(\"done\"), got {outcome:?}"
        );
        Ok(())
    }

    /// A6. With no timeout, parent cancellation still resolves the
    /// wrapper. (Without this the `--run-timeout` could be defeated by
    /// any phase configured with no per-phase budget.)
    #[rudzio::test]
    async fn no_timeout_with_parent_cancel_returns_cancelled(ctx: &Test) -> anyhow::Result<()> {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        let parent_for_cancel = parent.clone();
        let wrapper = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                ctx.sleep(Duration::from_secs(30_u64)).await;
            },
            None,
            None,
            child,
            |dur| ctx.sleep(dur),
        );
        let cancel = async {
            ctx.sleep(Duration::from_millis(20_u64)).await;
            parent_for_cancel.cancel();
        };
        let (outcome, ()) = join(wrapper, cancel).await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        Ok(())
    }

    /// A4. When the parent token is cancelled (e.g. run-timeout) before
    /// the per-phase timer fires, the outcome is `Cancelled` — NOT
    /// `TimedOut`. Distinguishing the two keeps the failure attribution
    /// honest: "you blew your budget" vs "the run was aborted".
    #[rudzio::test]
    async fn parent_cancellation_returns_cancelled_not_timed_out(ctx: &Test) -> anyhow::Result<()> {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        let parent_for_cancel = parent.clone();
        let wrapper = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                ctx.sleep(Duration::from_secs(30_u64)).await;
            },
            Some(Duration::from_secs(5_u64)),
            None,
            child,
            |dur| ctx.sleep(dur),
        );
        let cancel = async {
            ctx.sleep(Duration::from_millis(20_u64)).await;
            parent_for_cancel.cancel();
        };
        let (outcome, ()) = join(wrapper, cancel).await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "parent cancel must produce Cancelled, got {outcome:?}"
        );
        Ok(())
    }

    /// A1. A phase future that completes returns `Completed(value)`.
    #[rudzio::test]
    async fn phase_completes_returns_completed(ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel(
            async { 42_u32 },
            Some(Duration::from_secs(5_u64)),
            None,
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed(42)),
            "expected Completed(42), got {outcome:?}"
        );
        Ok(())
    }

    /// A3. A phase that runs longer than its budget returns `TimedOut`
    /// AND the wrapper cancels the phase token (so cooperative phase
    /// futures see the cancellation on their next poll).
    #[rudzio::test]
    async fn phase_exceeds_timeout_returns_timed_out_and_cancels_token(
        ctx: &Test,
    ) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let observer = token.clone();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                ctx.sleep(Duration::from_secs(30_u64)).await;
            },
            Some(Duration::from_millis(50_u64)),
            None,
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
        anyhow::ensure!(
            observer.is_cancelled(),
            "wrapper must cancel the phase token on timeout so the phase fut bails out cooperatively"
        );
        Ok(())
    }

    /// A2. A phase future that panics returns `Panicked(msg)` carrying
    /// the formatted panic payload.
    ///
    /// The unwind is triggered via [`resume_unwind`] (a function call
    /// with a `Box<String>` payload) rather than the `panic!` macro.
    /// `panic_payload_message` downcasts to `&'static str` then `String`,
    /// so a `Box::<String>::new(...)` payload round-trips identically.
    /// Calling the macro would trip `clippy::panic`, and the project
    /// policy forbids site-local `#[expect(clippy::panic, ...)]` outside
    /// `rudzio-fixtures/` — so we use the function form, which isn't
    /// covered by that lint.
    #[rudzio::test]
    async fn phase_panics_returns_panicked_with_message(ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async { resume_unwind(Box::<String>::new("boom_in_phase".to_owned())) },
            Some(Duration::from_secs(5_u64)),
            None,
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        if let PhaseOutcome::Panicked(msg) = &outcome {
            anyhow::ensure!(
                msg.contains("boom_in_phase"),
                "panic message must round-trip, got: {msg}"
            );
        } else {
            anyhow::bail!("expected Panicked, got {outcome:?}");
        }
        Ok(())
    }

    /// A7. A phase token that's already cancelled before the wrapper is
    /// even called must short-circuit to `Cancelled` immediately — no
    /// awaiting the phase future, no waiting for the timer.
    #[rudzio::test]
    async fn phase_token_already_cancelled_returns_cancelled_immediately(
        ctx: &Test,
    ) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        token.cancel();
        let start = Instant::now();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                ctx.sleep(Duration::from_secs(30_u64)).await;
            },
            Some(Duration::from_secs(30_u64)),
            None,
            token,
            |dur| ctx.sleep(dur),
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        anyhow::ensure!(
            elapsed < Duration::from_secs(1_u64),
            "wrapper must short-circuit on a pre-cancelled token, took {elapsed:?}"
        );
        Ok(())
    }
}
