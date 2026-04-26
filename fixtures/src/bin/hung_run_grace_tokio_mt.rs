//! Layer 1 fixture: a sync-blocking test body combined with
//! `--phase-hang-grace=0` (Layer 2 disabled) and `--run-timeout=1`.
//!
//! Sequence: run-timeout fires → root token cancelled → dispatch
//! loop tries to wind down → body never yields → without Layer 1
//! the binary hangs forever. With Layer 1 + `--cancel-grace-period=1`,
//! the watchdog thread observes the root token cancelled, sleeps 1s,
//! then `process::exit(2)` and prints a diagnostic to stderr.
//!
//! Wall-clock budget for the test harness: ≤ 4s.
//! Expected exit code: 2.

use std::thread::sleep as thread_sleep;
use std::time::Duration;

use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;
use tokio::task::spawn_blocking;

/// Sync sleep duration used inside the body's `spawn_blocking`. One
/// minute, expressed via `from_mins` — long enough that the
/// run-timeout, grace, and watchdog all fire before the sleep would
/// finish, even on a heavily loaded CI host.
const SYNC_SLEEP: Duration = Duration::from_mins(1_u64);

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::{SYNC_SLEEP, Test, spawn_blocking, thread_sleep};

    /// Body sync-sleeps for 60s on a blocking thread. With
    /// `--phase-hang-grace=0`, the body wrapper does not escalate to
    /// `[HANG]` itself — it returns `[TIMEOUT]` (or, more accurately,
    /// the run-timeout fires first and the per-test wrappers see
    /// `Cancelled`). Either way, the spawned-blocking thread keeps
    /// running. Layer 1 is what saves us.
    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts Layer 1's cancel-grace watchdog force-exits the process before the body completes; the println! is the unreached marker that the integration test greps for absence"
    )]
    async fn body_sync_blocks_60s(_ctx: &Test) -> anyhow::Result<()> {
        let _unused = spawn_blocking(|| {
            thread_sleep(SYNC_SLEEP);
        })
        .await;
        println!("body_sync_blocks_60s_unreached_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
