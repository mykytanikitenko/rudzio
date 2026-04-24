//! Per-test timeout fixture.
//!
//! The runner is invoked with `--test-timeout=1` but the test body
//! cooperatively awaits on its context's cancellation token; the runner's
//! per-test timer fires via `sleep_dyn` and drops the test future, producing
//! a `FAILED (timed out)` outcome. The subsequent passing test in the same
//! suite still runs, proving the timeout does not kill the whole run.

use std::time::Duration;

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::{Duration, Test};

    #[rudzio::test]
    async fn hangs_until_timeout(ctx: &Test) -> anyhow::Result<()> {
        // Sleeps past the configured 1-second `--test-timeout`. When the
        // runner's per-test watchdog fires it drops this future, so reaching
        // the `println!` below would be a bug — the integration assertion
        // asserts the absence of that marker.
        let _unused = ctx
            .cancel_token()
            .run_until_cancelled(async {
                ::tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("hangs_until_timeout_unreached_marker");
        Ok(())
    }

    #[rudzio::test]
    async fn still_runs_after_previous_timeout(_ctx: &Test) -> anyhow::Result<()> {
        println!("still_runs_after_previous_timeout_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
