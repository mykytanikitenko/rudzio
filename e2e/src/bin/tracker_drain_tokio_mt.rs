//! Verifies that `common_context::Global::teardown` drains the shared
//! `TaskTracker` before returning. If it didn't, the runtime would be
//! dropped while the tracked task is still sleeping, the task would be
//! cancelled, and the marker log would never reach stdout.
//!
//! Structure:
//!   - The test body spawns a tracked future and returns immediately
//!     (we explicitly drop the `JoinHandle` wrapper so we don't await it).
//!   - The future sleeps 60ms, then emits the marker log.
//!   - The integration test asserts that the marker is present.

use std::time::Duration;

use common_context::Test;
use rudzio::runtime::tokio::Multithread;
use tokio::time::sleep;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::{Duration, Test, sleep};

    #[rudzio::test]
    fn returns_before_tracked_task_completes(ctx: &Test) -> anyhow::Result<()> {
        let _join = ctx.spawn_tracked(async move {
            sleep(Duration::from_millis(60)).await;
            println!("tracker_drain_marker");
        });
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
