//! Per-test attribute body timeout override.
//!
//! No CLI timeouts. Test fn is annotated `#[rudzio::test(timeout = 1)]`,
//! body sleeps 30s cooperatively. The macro-emitted override produces
//! `Some(Duration::from_secs(1))` regardless of the missing CLI default,
//! so the body times out and the next test still runs unbounded.

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

    #[rudzio::test(timeout = 1)]
    async fn attr_body_times_out(ctx: &Test) -> anyhow::Result<()> {
        let _unused = ctx
            .cancel_token()
            .run_until_cancelled(async {
                ::tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .await;
        println!("attr_body_times_out_unreached_marker");
        Ok(())
    }

    #[rudzio::test]
    async fn unbounded_sibling_still_runs(_ctx: &Test) -> anyhow::Result<()> {
        println!("unbounded_sibling_marker");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
