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
    fn sync_panics(_ctx: &Test) -> anyhow::Result<()> {
        panic!("intentional sync panic")
    }
}

#[rudzio::main]
fn main() {}
