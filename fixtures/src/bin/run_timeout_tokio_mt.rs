//! Run-timeout fixture.
//!
//! Launched with `--run-timeout=1`. The first test blocks on its cancellation
//! token and returns gracefully as soon as the runner's watchdog cancels the
//! root token. The remaining queued tests are never started — the runner
//! reports them as `cancelled`.

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
        reason = "this fixture asserts the run-timeout watchdog cancels the root token; the println! emits the acknowledgement marker the integration test greps for"
    )]
    async fn waits_for_run_cancel(ctx: &Test) -> anyhow::Result<()> {
        // Stays pending until the runner's run-timeout watchdog cancels the
        // root token, at which point `run_until_cancelled` resolves with
        // `None` and the test returns `Ok(())` cooperatively.
        let completed: Option<()> = ctx
            .cancel_token()
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30)).await;
            })
            .await;
        if completed.is_none() {
            println!("waits_for_run_cancel_acknowledged_marker");
        }
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts queued tests after a run-timeout cancellation are never started; reaching the println! marker would be a bug the integration test greps for, so the marker must remain"
    )]
    async fn never_starts_first(_ctx: &Test) -> anyhow::Result<()> {
        println!("never_starts_first_unreached_marker");
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts queued tests after a run-timeout cancellation are never started; reaching the println! marker would be a bug the integration test greps for, so the marker must remain"
    )]
    async fn never_starts_second(_ctx: &Test) -> anyhow::Result<()> {
        println!("never_starts_second_unreached_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
