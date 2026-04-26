//! Verifies that `rudzio::common::context::Suite::teardown` drains the shared
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
        clippy::unnecessary_wraps,
        reason = "this fixture verifies Suite::teardown drains the shared TaskTracker before returning; the tracked task prints a marker that integration tests grep for, and the framework requires the test fn signature to return anyhow::Result<()>"
    )]
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
