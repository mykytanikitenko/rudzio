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
//!
//! The intentional unwind in `phase_panics_returns_panicked_with_message`
//! is triggered via [`std::panic::resume_unwind`] (a function call) rather
//! than the `panic!` macro. Both produce identical observable behaviour
//! through `catch_unwind` / `panic_payload_message`, but only the macro
//! form trips `clippy::panic`. Using `resume_unwind` lets the test stay
//! honest (real unwind, framework genuinely captures it) without a
//! site-local `#[expect]` escape hatch.

use std::panic::resume_unwind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rudzio::common::context::{Suite, Test};
use rudzio::futures_util::future::{AbortHandle, Abortable, Aborted};
use rudzio::runtime::tokio::Multithread;
use rudzio::suite::{PhaseOutcome, drive_per_test_spawn, run_phase_with_timeout_and_cancel};
use rudzio::tokio::runtime::Handle;
use rudzio::tokio::spawn as tokio_spawn;
use rudzio::tokio::task::{spawn_blocking, yield_now};
use rudzio::tokio::time::sleep;
use rudzio::tokio_util::sync::CancellationToken;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
mod phase_wrapper_tests {
    use super::{
        AbortHandle, Abortable, Aborted, Arc, AtomicBool, CancellationToken, Duration, Handle,
        Instant, Ordering, PhaseOutcome, Test, drive_per_test_spawn, resume_unwind,
        run_phase_with_timeout_and_cancel, sleep, spawn_blocking, thread, tokio_spawn, yield_now,
    };

    /// L3.3. A cooperatively-cancellable body whose cancellation lands
    /// before the grace window expires must come back as `TimedOut`,
    /// not `Hung`. The driver's grace step is for *escalating* a
    /// stuck body — it must NOT downgrade fast cooperative timeouts.
    #[rudzio::test]
    async fn drive_per_test_spawn_cooperative_timeout(_ctx: &Test) -> anyhow::Result<()> {
        let rt = Handle::current();
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
                Ok(inner) => inner,
                Err(err) => {
                    if err.is_cancelled() {
                        Err(Aborted)
                    } else {
                        resume_unwind(err.into_panic())
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
            Some(Duration::from_millis(50_u64)),
            Some(Duration::from_secs(2_u64)),
            body_token,
            sleep,
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
    async fn drive_per_test_spawn_completed_body(_ctx: &Test) -> anyhow::Result<()> {
        let rt = Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let join = rt.spawn(Abortable::new(async { 99_u32 }, abort_reg));
        let join_fut = async move {
            match join.await {
                Ok(inner) => inner,
                Err(err) => resume_unwind(err.into_panic()),
            }
        };
        let token = CancellationToken::new();
        let outcome = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_secs(5_u64)),
            Some(Duration::from_millis(200_u64)),
            token,
            sleep,
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Completed(99)),
            "expected Completed(99), got {outcome:?}"
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
    async fn drive_per_test_spawn_hung_fires_abort_handle(_ctx: &Test) -> anyhow::Result<()> {
        let rt = Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let observed_abort = Arc::new(AtomicBool::new(false));
        let observed_abort_for_task = Arc::clone(&observed_abort);
        let body = async move {
            // Cooperatively-pollable body: yields on every poll. The
            // driver's abort signal will flip the Abortable flag, so
            // the next `yield_now().await` returns `Err(Aborted)` from
            // the Abortable wrapper.
            for _ in 0_u32..1_000_000_u32 {
                sleep(Duration::from_millis(10_u64)).await;
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
                Ok(inner) => inner,
                Err(err) => {
                    if err.is_cancelled() {
                        Err(Aborted)
                    } else {
                        resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let token = CancellationToken::new();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(50_u64)),
            Some(Duration::from_millis(50_u64)),
            token,
            sleep,
        )
        .await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Hung),
            "expected Hung, got {outcome:?}"
        );
        // Give the spawn one more poll cycle so its post-abort cleanup
        // observes the aborted flag and stores `true`.
        sleep(Duration::from_millis(100_u64)).await;
        anyhow::ensure!(
            observed_abort.load(Ordering::SeqCst),
            "abort handle must have been fired before driver returned Hung"
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
    async fn drive_per_test_spawn_hung_uncooperative_body(_ctx: &Test) -> anyhow::Result<()> {
        let rt = Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body = async {
            // Yield once so Abortable polls the inner future at least
            // once; thereafter spawn_blocking keeps the body in flight
            // without yielding back.
            yield_now().await;
            let _unused = spawn_blocking(|| {
                thread::sleep(Duration::from_secs(30_u64));
            })
            .await;
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                Ok(inner) => inner,
                Err(err) => {
                    if err.is_cancelled() {
                        Err(Aborted)
                    } else {
                        resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let token = CancellationToken::new();
        let start = Instant::now();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(200_u64)),
            Some(Duration::from_millis(300_u64)),
            token,
            sleep,
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Hung),
            "expected Hung, got {outcome:?}"
        );
        // Generous wall-clock bound: the budget+grace adds up to
        // 500ms of *intended* wait, but under the workspace
        // aggregator (`cargo rudzio test ./`) many tokio runtimes
        // run concurrently and timer wakes can be late by a couple
        // of seconds. The correctness signal we care about is the
        // `Hung` outcome above — that proves the driver did not
        // block on the body. The bound here is just a generous
        // sanity check that the driver isn't waiting for the full
        // 30s sleep.
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
    async fn drive_per_test_spawn_no_budget_awaits_to_completion(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let rt = Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body = async {
            sleep(Duration::from_millis(50_u64)).await;
            "done"
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                Ok(inner) => inner,
                Err(err) => resume_unwind(err.into_panic()),
            }
        };
        let token = CancellationToken::new();
        let outcome = drive_per_test_spawn(join_fut, abort_handle, None, None, token, sleep).await;
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
    /// hardkill.
    #[rudzio::test]
    async fn drive_per_test_spawn_no_grace_returns_timed_out(_ctx: &Test) -> anyhow::Result<()> {
        let rt = Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body = async {
            yield_now().await;
            let _unused = spawn_blocking(|| {
                thread::sleep(Duration::from_secs(30_u64));
            })
            .await;
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                Ok(inner) => inner,
                Err(err) => {
                    if err.is_cancelled() {
                        Err(Aborted)
                    } else {
                        resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let token = CancellationToken::new();
        let start = Instant::now();
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_millis(100_u64)),
            None, // grace disabled
            token,
            sleep,
        )
        .await;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::TimedOut),
            "expected TimedOut, got {outcome:?}"
        );
        // Generous wall-clock bound: see the comment in
        // `drive_per_test_spawn_hung_uncooperative_body` above. The
        // driver returns on budget expiry without waiting for the
        // body — the correctness signal is the `TimedOut` outcome
        // above. We just sanity-check the driver isn't waiting for
        // the full 30s sleep to finish.
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
    /// phase blew its budget".
    #[rudzio::test]
    async fn drive_per_test_spawn_parent_cancellation(_ctx: &Test) -> anyhow::Result<()> {
        let rt = Handle::current();
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        let body_token = CancellationToken::new();
        let body_token_for_task = body_token.clone();
        let body = async move {
            body_token_for_task.cancelled().await;
        };
        let join = rt.spawn(Abortable::new(body, abort_reg));
        let join_fut = async move {
            match join.await {
                Ok(inner) => inner,
                Err(err) => {
                    if err.is_cancelled() {
                        Err(Aborted)
                    } else {
                        resume_unwind(err.into_panic())
                    }
                }
            }
        };
        let body_token_for_cancel = body_token.clone();
        let cancel_in = tokio_spawn(async move {
            sleep(Duration::from_millis(20_u64)).await;
            body_token_for_cancel.cancel();
        });
        let outcome: PhaseOutcome<()> = drive_per_test_spawn(
            join_fut,
            abort_handle,
            Some(Duration::from_secs(5_u64)),
            Some(Duration::from_millis(300_u64)),
            body_token,
            sleep,
        )
        .await;
        let _unused = cancel_in.await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        Ok(())
    }

    /// A5. With no timeout (`None`), the wrapper still awaits the phase
    /// future and returns `Completed` when it resolves.
    #[rudzio::test]
    async fn no_timeout_with_completion_returns_completed(_ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome =
            run_phase_with_timeout_and_cancel(async { "done" }, None, None, token, sleep).await;
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
        let cancel_in = tokio_spawn(async move {
            sleep(Duration::from_millis(20_u64)).await;
            parent_for_task.cancel();
        });
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                sleep(Duration::from_secs(30_u64)).await;
            },
            None,
            None,
            child,
            sleep,
        )
        .await;
        let _unused = cancel_in.await;
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
    async fn parent_cancellation_returns_cancelled_not_timed_out(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        let parent_for_task = parent.clone();
        let cancel_in = tokio_spawn(async move {
            sleep(Duration::from_millis(20_u64)).await;
            parent_for_task.cancel();
        });
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                sleep(Duration::from_secs(30_u64)).await;
            },
            Some(Duration::from_secs(5_u64)),
            None,
            child,
            sleep,
        )
        .await;
        let _unused = cancel_in.await;
        anyhow::ensure!(
            matches!(outcome, PhaseOutcome::Cancelled),
            "parent cancel must produce Cancelled, got {outcome:?}"
        );
        Ok(())
    }

    /// A1. A phase future that completes returns `Completed(value)`.
    #[rudzio::test]
    async fn phase_completes_returns_completed(_ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel(
            async { 42_u32 },
            Some(Duration::from_secs(5_u64)),
            None,
            token,
            sleep,
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
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let observer = token.clone();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                sleep(Duration::from_secs(30_u64)).await;
            },
            Some(Duration::from_millis(50_u64)),
            None,
            token,
            sleep,
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
    async fn phase_panics_returns_panicked_with_message(_ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async { resume_unwind(Box::<String>::new("boom_in_phase".to_owned())) },
            Some(Duration::from_secs(5_u64)),
            None,
            token,
            sleep,
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
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        token.cancel();
        let start = Instant::now();
        let outcome = run_phase_with_timeout_and_cancel::<_, (), _, _>(
            async {
                sleep(Duration::from_secs(30_u64)).await;
            },
            Some(Duration::from_secs(30_u64)),
            None,
            token,
            sleep,
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
