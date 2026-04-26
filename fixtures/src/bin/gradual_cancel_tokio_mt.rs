//! Gradual cancellation fixture.
//!
//! The test spawns a tracked background task that awaits the context's cancel
//! token, prints a cleanup marker once the token fires, and then returns. The
//! run is driven with `--run-timeout=1`, so the root token is cancelled while
//! the task is still in-flight. Suite teardown calls `tracker.wait()`, which
//! cannot return until the tracked task finishes — the marker therefore
//! always appears in the output before the process exits.

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
    use super::{Duration, Test, sleep};

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture verifies that a tracked task's cleanup marker reaches stdout before the process exits even when the root cancel token fires mid-task; integration tests grep for the marker"
    )]
    async fn task_cleans_up_on_cancel(ctx: &Test) -> anyhow::Result<()> {
        let token = ctx.cancel_token().clone();
        // `spawn_tracked` is eager: the inner `rt.spawn` runs synchronously,
        // so dropping the returned join future is fine — the task is already
        // in the pool and tracked.
        drop(ctx.spawn_tracked(async move {
            token.cancelled().await;
            // Simulate a little graceful shutdown work before the marker.
            sleep(Duration::from_millis(50)).await;
            println!("gradual_cancel_cleanup_marker");
        }));

        // Hold the test open until cancellation arrives so the tracked task
        // is still running when the root token fires.
        let _unused = ctx
            .cancel_token()
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30)).await;
            })
            .await;

        Ok(())
    }
}

#[rudzio::main]
fn main() {}
