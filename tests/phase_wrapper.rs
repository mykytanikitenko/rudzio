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
//! escalates from `TimedOut` to `Hung` and the abort signal is fired so
//! tokio drops the task on its next poll.
//!
//! `tokio::time::sleep` is fine here — the wrappers themselves are
//! runtime-agnostic, and Multithread is the simplest harness for `await`
//! semantics. The wall-clock waits are bounded to the sub-second range so
//! the suite stays fast.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rudzio::futures_util::future::{AbortHandle, Abortable, Aborted};
use rudzio::suite::{PhaseOutcome, drive_per_test_spawn, run_phase_with_timeout_and_cancel};
use rudzio::tokio_util::sync::CancellationToken;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod phase_wrapper_tests {
    use super::{
        AbortHandle, Abortable, Aborted, Arc, AtomicBool, CancellationToken, Duration, Ordering,
        PhaseOutcome, drive_per_test_spawn, run_phase_with_timeout_and_cancel,
    };
    use rudzio::common::context::Test;

    /// A1. A phase future that completes returns `Completed(value)`.
    #[rudzio::test]
    async fn phase_completes_returns_completed(_ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel(
            async { 42_u32 },
            Some(Duration::from_secs(5)),
            None,
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed(42)),
            "expected Completed(42), got {outcome:?}"
        );
        Ok(())
    }

    /// A2. A phase future that panics returns `Panicked(msg)` carrying
    /// the formatted panic payload.
    #[rudzio::test]
    async fn phase_panics_returns_panicked_with_message(_ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _>(
            async { panic!("boom_in_phase") },
            Some(Duration::from_secs(5)),
            None,
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        match outcome {
            PhaseOutcome::Panicked(msg) => {
                anyhow::ensure!(
                    msg.contains("boom_in_phase"),
                    "panic message must round-trip, got: {msg}"
                );
            }
            other => anyhow::bail!("expected Panicked, got {other:?}"),
        }
        Ok(())
    }

    /// A3. A phase that runs longer than its budget returns `TimedOut`
    /// AND the wrapper cancels the phase token (so cooperative phase
    /// futures see the cancellation on their next poll).
    #[rudzio::test]
    async fn phase_exceeds_timeout_returns_timed_out_and_cancels_token(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let observer = token.clone();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _>(
            async {
                ::rudzio::tokio::time::sleep(Duration::from_secs(30)).await;
            },
            Some(Duration::from_millis(50)),
            None,
            token,
            ::rudzio::tokio::time::sleep,
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

    /// A4. When the parent token is cancelled (e.g. run-timeout) before
    /// the per-phase timer fires, the outcome is `Cancelled` — NOT
    /// `TimedOut`. Distinguishing the two keeps the failure attribution
    /// honest: "you blew your budget" vs "the run was aborted".
    #[rudzio::test]
    async fn parent_cancellation_returns_cancelled_not_timed_out(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        let parent_for_task = parent.clone();
        let cancel_in = ::rudzio::tokio::spawn(async move {
            ::rudzio::tokio::time::sleep(Duration::from_millis(20)).await;
            parent_for_task.cancel();
        });
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _>(
            async {
                ::rudzio::tokio::time::sleep(Duration::from_secs(30)).await;
            },
            Some(Duration::from_secs(5)),
            None,
            child,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        let _unused = cancel_in.await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "parent cancel must produce Cancelled, got {outcome:?}"
        );
        Ok(())
    }

    /// A5. With no timeout (`None`), the wrapper still awaits the phase
    /// future and returns `Completed` when it resolves.
    #[rudzio::test]
    async fn no_timeout_with_completion_returns_completed(_ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel(
            async { "done" },
            None,
            None,
            token,
            ::rudzio::tokio::time::sleep,
        )
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
    async fn no_timeout_with_parent_cancel_returns_cancelled(_ctx: &Test) -> anyhow::Result<()> {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        let parent_for_task = parent.clone();
        let cancel_in = ::rudzio::tokio::spawn(async move {
            ::rudzio::tokio::time::sleep(Duration::from_millis(20)).await;
            parent_for_task.cancel();
        });
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _>(
            async {
                ::rudzio::tokio::time::sleep(Duration::from_secs(30)).await;
            },
            None,
            None,
            child,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        let _unused = cancel_in.await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        Ok(())
    }

    /// A7. A phase token that's already cancelled before the wrapper is
    /// even called must short-circuit to `Cancelled` immediately — no
    /// awaiting the phase future, no waiting for the timer.
    #[rudzio::test]
    async fn phase_token_already_cancelled_returns_cancelled_immediately(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        token.cancel();
        let start = ::std::time::Instant::now();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _>(
            async {
                ::rudzio::tokio::time::sleep(Duration::from_secs(30)).await;
            },
            Some(Duration::from_secs(30)),
            None,
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        anyhow::ensure!(
            elapsed < Duration::from_secs(1),
            "wrapper must short-circuit on a pre-cancelled token, took {elapsed:?}"
        );
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Layer 3 — `drive_per_test_spawn` driver tests.
    //
    // These tests cover the spawned-body path: the body has been handed
    // off to `Runtime::spawn` wrapped in `Abortable`, and the wrapper
    // outside the spawn is `drive_per_test_spawn`. The driver races the
    // spawn's `JoinFuture` against the per-phase timeout, then (on
    // timeout) cancels the phase token, fires the abort handle, and
    // waits up to `outer_grace` for the spawn to finish before
    // declaring `Hung` and returning.
    //
    // We need a tokio multithread runtime in scope — the macro already
    // hands us one as `rt: &Multithread`. Suite::context's `Test` borrow
    // gives us access to it via `rt` indirectly but we re-fetch via
    // `Runtime::sleep`-shaped APIs in the suite annotation runtime
    // rather than reaching for a fresh runtime.
    // ---------------------------------------------------------------------

    /// L3.1. A spawned body that completes promptly resolves the driver
    /// with `Completed(value)` carrying the body's return value. The
    /// abort handle is never fired; cancelling it post-completion is a
    /// no-op.
    #[rudzio::test]
    async fn drive_per_test_spawn_completed_body(_ctx: &Test) -> anyhow::Result<()> {
        let rt = ::rudzio::tokio::runtime::Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let join = rt.spawn(Abortable::new(async { 99_u32 }, abort_reg));
        let join_fut = async move {
            match join.await {
                ::std::result::Result::Ok(inner) => inner,
                ::std::result::Result::Err(err) => {
                    ::std::panic::resume_unwind(err.into_panic())
                }
            }
        };
        let token = CancellationToken::new();
        let outcome = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_secs(5)),
            Some(Duration::from_millis(200)),
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed(99)),
            "expected Completed(99), got {outcome:?}"
        );
        Ok(())
    }

    /// L3.2. An uncooperative spawned body — one that ignores both the
    /// phase token and the abort handle — must still be reported as
    /// `Hung` after the outer grace window. Use a sync-blocking
    /// `std::thread::sleep` inside the spawn so the body genuinely
    /// cannot be aborted: tokio's abort flag is checked only at poll
    /// boundaries, and a sync sleep yields no boundary. The driver
    /// proves it can move on regardless: wall-clock total ≤
    /// budget + grace + 0.5s slack.
    ///
    /// We deliberately spawn the sleep on `tokio::task::spawn_blocking`
    /// so it runs on the blocking pool rather than starving a worker
    /// thread the test framework itself relies on. The driver still
    /// observes "spawn never finishes within budget+grace", which is
    /// what `Hung` is supposed to detect.
    #[rudzio::test]
    async fn drive_per_test_spawn_hung_uncooperative_body(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let rt = ::rudzio::tokio::runtime::Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body = async {
            // Yield once so Abortable polls the inner future at least
            // once; thereafter spawn_blocking keeps the body in flight
            // without yielding back.
            ::rudzio::tokio::task::yield_now().await;
            let _unused = ::rudzio::tokio::task::spawn_blocking(|| {
                ::std::thread::sleep(Duration::from_secs(30));
            })
            .await;
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                ::std::result::Result::Ok(inner) => inner,
                ::std::result::Result::Err(err) => {
                    if err.is_cancelled() {
                        ::std::result::Result::Err(Aborted)
                    } else {
                        ::std::panic::resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let token = CancellationToken::new();
        let start = ::std::time::Instant::now();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(200)),
            Some(Duration::from_millis(300)),
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Hung),
            "expected Hung, got {outcome:?}"
        );
        anyhow::ensure!(
            elapsed < Duration::from_millis(1500),
            "driver must NOT block on the uncooperative body, took {elapsed:?}"
        );
        Ok(())
    }

    /// L3.3. A cooperatively-cancellable body whose cancellation lands
    /// before the grace window expires must come back as `TimedOut`,
    /// not `Hung`. The driver's grace step is for *escalating* a
    /// stuck body — it must NOT downgrade fast cooperative timeouts.
    #[rudzio::test]
    async fn drive_per_test_spawn_cooperative_timeout(_ctx: &Test) -> anyhow::Result<()> {
        let rt = ::rudzio::tokio::runtime::Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body_token = CancellationToken::new();
        let body_token_for_task = body_token.clone();
        let body = async move {
            // Cooperatively responds to cancel: returns immediately on
            // cancel signal.
            body_token_for_task.cancelled().await;
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                ::std::result::Result::Ok(inner) => inner,
                ::std::result::Result::Err(err) => {
                    if err.is_cancelled() {
                        ::std::result::Result::Err(Aborted)
                    } else {
                        ::std::panic::resume_unwind(err.into_panic())
                    }
                }
            }
        };
        // The phase_token passed into the driver is what the driver
        // cancels on timeout. We bridge the body to the phase token by
        // cloning it: the driver cancels it, the body sees it.
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(50)),
            Some(Duration::from_secs(2)),
            body_token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::TimedOut),
            "expected TimedOut (grace honoured cooperative cancel), got {outcome:?}"
        );
        Ok(())
    }

    /// L3.4. With `outer_grace = None` (Layer 2 disabled), the driver
    /// returns `TimedOut` immediately on budget expiry — no grace
    /// window, no escalation to `Hung`. This is the opt-out path for
    /// users who'd rather rely on Layer 1 (`process::exit(2)`) for
    /// hardkill.
    #[rudzio::test]
    async fn drive_per_test_spawn_no_grace_returns_timed_out(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let rt = ::rudzio::tokio::runtime::Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body = async {
            ::rudzio::tokio::task::yield_now().await;
            let _unused = ::rudzio::tokio::task::spawn_blocking(|| {
                ::std::thread::sleep(Duration::from_secs(30));
            })
            .await;
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                ::std::result::Result::Ok(inner) => inner,
                ::std::result::Result::Err(err) => {
                    if err.is_cancelled() {
                        ::std::result::Result::Err(Aborted)
                    } else {
                        ::std::panic::resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let token = CancellationToken::new();
        let start = ::std::time::Instant::now();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(100)),
            None, // grace disabled
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
        anyhow::ensure!(
            elapsed < Duration::from_millis(800),
            "driver must return on budget expiry without waiting for grace, took {elapsed:?}"
        );
        Ok(())
    }

    /// L3.5. When the parent token is cancelled before the budget
    /// fires, the driver returns `Cancelled` (not `TimedOut`, not
    /// `Hung`). Same distinction the inline wrapper makes: parent
    /// cancellation is "someone above pulled the plug", not "this
    /// phase blew its budget".
    #[rudzio::test]
    async fn drive_per_test_spawn_parent_cancellation(_ctx: &Test) -> anyhow::Result<()> {
        let rt = ::rudzio::tokio::runtime::Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body_token = CancellationToken::new();
        let body_token_for_task = body_token.clone();
        let body = async move {
            body_token_for_task.cancelled().await;
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                ::std::result::Result::Ok(inner) => inner,
                ::std::result::Result::Err(err) => {
                    if err.is_cancelled() {
                        ::std::result::Result::Err(Aborted)
                    } else {
                        ::std::panic::resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let body_token_for_cancel = body_token.clone();
        let cancel_in = ::rudzio::tokio::spawn(async move {
            ::rudzio::tokio::time::sleep(Duration::from_millis(20)).await;
            body_token_for_cancel.cancel();
        });
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_secs(5)),
            Some(Duration::from_millis(300)),
            body_token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        let _unused = cancel_in.await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        Ok(())
    }

    /// L3.6. The driver must actually fire the abort handle on
    /// grace-expiry escalation. We observe the side effect via an
    /// `Abortable::is_aborted`-like pattern: spawn a body that polls
    /// indefinitely with `tokio::task::yield_now` and ALSO checks an
    /// `Arc<AtomicBool>` that we flip when we observe `Err(Aborted)`.
    /// After the driver returns `Hung`, awaiting the underlying
    /// `Abortable` future (post-driver) must yield `Err(Aborted)` —
    /// i.e. the abort was signalled.
    #[rudzio::test]
    async fn drive_per_test_spawn_hung_fires_abort_handle(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let rt = ::rudzio::tokio::runtime::Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let observed_abort = Arc::new(AtomicBool::new(false));
        let observed_abort_for_task = Arc::clone(&observed_abort);
        let body = async move {
            // Cooperatively-pollable body: yields on every poll. The
            // driver's abort signal will flip the Abortable flag, so
            // the next `yield_now().await` returns `Err(Aborted)` from
            // the Abortable wrapper.
            for _ in 0..1_000_000 {
                ::rudzio::tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let abortable = Abortable::new(body, abort_reg);
        let join = rt.spawn(async move {
            let result = abortable.await;
            if result.is_err() {
                observed_abort_for_task.store(true, Ordering::SeqCst);
            }
            result
        });
        let join_fut = async move {
            match join.await {
                ::std::result::Result::Ok(inner) => inner,
                ::std::result::Result::Err(err) => {
                    if err.is_cancelled() {
                        ::std::result::Result::Err(Aborted)
                    } else {
                        ::std::panic::resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let token = CancellationToken::new();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(50)),
            Some(Duration::from_millis(50)),
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Hung),
            "expected Hung, got {outcome:?}"
        );
        // Give the spawn one more poll cycle so its post-abort cleanup
        // observes the aborted flag and stores `true`.
        ::rudzio::tokio::time::sleep(Duration::from_millis(100)).await;
        anyhow::ensure!(
            observed_abort.load(Ordering::SeqCst),
            "abort handle must have been fired before driver returned Hung"
        );
        Ok(())
    }

    /// L3.7. With `outer_budget = None` and a parent token never
    /// cancelled, the driver awaits the spawn until completion — no
    /// timer, no grace, just plain await. Confirms the driver doesn't
    /// short-circuit when timeouts are unconfigured.
    #[rudzio::test]
    async fn drive_per_test_spawn_no_budget_awaits_to_completion(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let rt = ::rudzio::tokio::runtime::Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body = async {
            ::rudzio::tokio::time::sleep(Duration::from_millis(50)).await;
            "done"
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                ::std::result::Result::Ok(inner) => inner,
                ::std::result::Result::Err(err) => {
                    ::std::panic::resume_unwind(err.into_panic())
                }
            }
        };
        let token = CancellationToken::new();
        let outcome = drive_per_test_spawn(
            join_fut,
            abort_handle,
            None,
            None,
            token,
            ::rudzio::tokio::time::sleep,
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed("done")),
            "expected Completed(\"done\"), got {outcome:?}"
        );
        Ok(())
    }
}
