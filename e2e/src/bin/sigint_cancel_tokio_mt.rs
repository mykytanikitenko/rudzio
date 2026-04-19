//! SIGINT cancellation fixture.
//!
//! The first test awaits its cancellation token (set up to be a child of the
//! runner's root token) and reports that it observed cancellation. Subsequent
//! tests are queued but should never start — the integration test sends
//! SIGINT to the process while the first test is sleeping, and asserts that
//! the run exits gracefully with the remaining tests marked `cancelled`.

use std::time::Duration;

use common_context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::{Duration, Test};

    #[rudzio::test]
    async fn waits_for_sigint(ctx: &Test) -> anyhow::Result<()> {
        // Tell the outer process it is safe to send SIGINT now — waiting on
        // this marker avoids a race where the signal is delivered before the
        // runner's ctrlc handler has been installed.
        println!("sigint_cancel_ready_marker");
        let completed = ctx
            .cancel_token()
            .run_until_cancelled(async {
                ::tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
        if completed.is_none() {
            println!("sigint_cancel_observed_marker");
        }
        Ok(())
    }

    #[rudzio::test]
    async fn never_runs_after_sigint(_ctx: &Test) -> anyhow::Result<()> {
        println!("never_runs_after_sigint_unreached_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
