//! Per-test attribute body timeout override.
//!
//! No CLI timeouts. Test fn is annotated `#[rudzio::test(timeout = 1)]`,
//! body sleeps 30s cooperatively. The macro-emitted override produces
//! `Some(Duration::from_secs(1))` regardless of the missing CLI default,
//! so the body times out and the next test still runs unbounded.

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

    #[rudzio::test(timeout = 1)]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture verifies the per-test #[rudzio::test(timeout=1)] override fires after 1s; the marker after the cancelled sleep must never appear, and integration tests grep stdout to confirm absence"
    )]
    async fn attr_body_times_out(ctx: &Test) -> anyhow::Result<()> {
        let _unused = ctx
            .cancel_token()
            .run_until_cancelled(async {
                sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("attr_body_times_out_unreached_marker");
        Ok(())
    }

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture verifies an unbounded sibling test runs unaffected after another test's per-test attribute timeout fires; integration tests grep stdout for this marker"
    )]
    async fn unbounded_sibling_still_runs(_ctx: &Test) -> anyhow::Result<()> {
        println!("unbounded_sibling_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
