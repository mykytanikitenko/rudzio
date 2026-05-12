//! Runtime coverage for sync (non-`async`) test bodies.
//!
//! Guards two fixes at once:
//!   - the sync arm no longer uses the broken `spawn_blocking` path;
//!   - `std::panic::catch_unwind` in the sync arm isolates panics so they
//!     don't kill the runtime thread.
//!
//! Scenario: three sync tests — one passes, one returns `Err`, one panics —
//! all should execute, and the summary should report 1/1/1 for passed /
//! failed / panicked with exit code 1.

use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture exercises the sync arm's std::panic::catch_unwind isolation; sync_panics() diverges so its anyhow::Result<()> wrapper is statically unreachable, and the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn sync_passes(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn sync_returns_err(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::bail!("intentional sync failure")
    }

    #[rudzio::test]
    #[expect(
        clippy::panic,
        reason = "this fixture exercises the sync arm's std::panic::catch_unwind isolation; the test body must panic to verify the runtime thread isn't killed"
    )]
    fn sync_panics(_ctx: &Test) -> anyhow::Result<()> {
        panic!("intentional sync panic")
    }
}

#[rudzio::main]
fn main() {}
