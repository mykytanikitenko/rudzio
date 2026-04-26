//! Unit tests for `rudzio::suite::run_phase_with_timeout_and_cancel`.
//!
//! The phase wrapper is the canonical primitive every phase (suite setup,
//! suite teardown, per-test setup, test body, per-test teardown) wraps
//! itself in: it races the phase future against an optional per-phase
//! timeout while staying responsive to a parent cancellation token.
//!
//! `tokio::time::sleep` is fine here — the wrapper itself is
//! runtime-agnostic, and Multithread is the simplest harness for `await`
//! semantics. The wall-clock waits are bounded to the sub-second range so
//! the suite stays fast.

use std::time::Duration;

use rudzio::suite::{PhaseOutcome, run_phase_with_timeout_and_cancel};
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
        CancellationToken, Duration, PhaseOutcome, run_phase_with_timeout_and_cancel,
    };
    use rudzio::common::context::Test;

    /// A1. A phase future that completes returns `Completed(value)`.
    #[rudzio::test]
    async fn phase_completes_returns_completed(_ctx: &Test) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let outcome = run_phase_with_timeout_and_cancel(
            async { 42_u32 },
            Some(Duration::from_secs(5)),
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
            elapsed < Duration::from_secs(1),
            "wrapper must short-circuit on a pre-cancelled token, took {elapsed:?}"
        );
        Ok(())
    }
}
