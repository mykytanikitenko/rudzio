//! Verifies that `rudzio::common::context::Test::teardown` cancels the per-test
//! cancellation token, waking any task that was waiting on it.
//!
//! Structure:
//!   - The test body spawns a tracked future that `await`s the cancel token,
//!     then emits a marker log.
//!   - The test body returns without cancelling the token itself.
//!   - When per-test teardown fires, the token is cancelled; the spawned
//!     future wakes up and logs the marker.
//!   - The suite tracker drain guarantees we see the marker before exit.

use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use rudzio::context::Test as _;

    use super::Test;

    #[rudzio::test]
    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts Test::teardown cancels the per-test token by emitting a machine-readable cancel_propagation_marker line on stdout that the integration test greps; println! is the deliberate channel"
    )]
    fn cancel_token_wakes_on_teardown(ctx: &Test) -> anyhow::Result<()> {
        let token = ctx.cancel_token().clone();
        let _join = ctx.spawn_tracked(async move {
            token.cancelled().await;
            println!("cancel_propagation_marker");
        });
        // Sanity-check: the token must NOT be cancelled while the test body
        // is running; the cancel has to come from `Test::teardown`.
        if ctx.cancel_token().is_cancelled() {
            anyhow::bail!("cancel token was set before teardown ran");
        }
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
