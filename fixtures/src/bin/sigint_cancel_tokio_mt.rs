//! SIGINT cancellation fixture.
//!
//! The first test awaits its cancellation token (set up to be a child of the
//! runner's root token) and reports that it observed cancellation. Subsequent
//! tests are queued but should never start — the integration test sends
//! SIGINT to the process while the first test is waiting, and asserts that
//! the run exits gracefully with the remaining tests marked `cancelled`.
//!
//! **Deterministic body:** `std::future::pending()` never completes by
//! itself, so the test can only resolve via the cancellation token.
//! That makes the outcome a strict function of cancellation propagation —
//! it never falsely "passes" by the wall-clock timer running out before
//! the signal arrives, even under saturated CPU contention.

use std::future::pending;

use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use rudzio::context::Test as _;

    use super::{Test, pending};

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture coordinates a SIGINT delivery from the integration test by emitting readiness/observed markers on stdout that the parent process greps; println! is the deliberate channel"
    )]
    async fn waits_for_sigint(ctx: &Test) -> anyhow::Result<()> {
        // Tell the outer process it is safe to send SIGINT now — waiting on
        // this marker avoids a race where the signal is delivered before the
        // runner's ctrlc handler has been installed.
        println!("sigint_cancel_ready_marker");
        // Body is `pending()` so the ONLY resolution path is the
        // cancellation token. `run_until_cancelled` returns
        // `None` exactly when the token fires; we print the
        // observed-marker on that path.
        let completed = ctx
            .cancel_token()
            .run_until_cancelled(pending::<()>())
            .await;
        if completed.is_none() {
            println!("sigint_cancel_observed_marker");
        }
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts the queued test never runs after SIGINT cancellation; the println! marker would only appear if the runner failed to honor cancellation"
    )]
    async fn never_runs_after_sigint(_ctx: &Test) -> anyhow::Result<()> {
        println!("never_runs_after_sigint_unreached_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
