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

    /// Body sync-sleeps for 60s on a blocking thread. With
    /// `--phase-hang-grace=0`, the body wrapper does not escalate to
    /// `[HANG]` itself — it returns `[TIMEOUT]` (or, more accurately,
    /// the run-timeout fires first and the per-test wrappers see
    /// `Cancelled`). Either way, the spawned-blocking thread keeps
    /// running. Layer 1 is what saves us.
    #[rudzio::test]
    async fn body_sync_blocks_60s(_ctx: &Test) -> anyhow::Result<()> {
        let _unused = ::tokio::task::spawn_blocking(|| {
            ::std::thread::sleep(Duration::from_secs(60));
        })
        .await;
        println!("body_sync_blocks_60s_unreached_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
