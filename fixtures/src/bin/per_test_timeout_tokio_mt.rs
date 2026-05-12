//! Per-test timeout fixture.
//!
//! The runner is invoked with `--test-timeout=1` but the test body
//! cooperatively awaits on its context's cancellation token; the runner's
//! per-test timer fires via `sleep_dyn` and drops the test future, producing
//! a `FAILED (timed out)` outcome. The subsequent passing test in the same
//! suite still runs, proving the timeout does not kill the whole run.

use std::time::Duration;

use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;
use tokio::time::sleep;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use rudzio::context::Test as _;

    use super::{Duration, Test, sleep};

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts the per-test watchdog drops the future before the body's println! runs; reaching that println! line is the bug the integration test greps for, so the marker must remain"
    )]
    async fn hangs_until_timeout(ctx: &Test) -> anyhow::Result<()> {
        // Sleeps past the configured 1-second `--test-timeout`. When the
        // runner's per-test watchdog fires it drops this future, so reaching
        // the `println!` below would be a bug — the integration assertion
        // asserts the absence of that marker.
        let _unused: Option<()> = ctx
            .cancel_token()
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("hangs_until_timeout_unreached_marker");
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts a passing test still runs after the previous test's per-test timeout; the println! emits the marker the integration test greps to confirm the run continued"
    )]
    async fn still_runs_after_previous_timeout(_ctx: &Test) -> anyhow::Result<()> {
        println!("still_runs_after_previous_timeout_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
