use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    use super::{Arc, AtomicBool, Ordering, Test};

    #[rudzio::test]
    async fn spawn_awaits_result(ctx: &Test) -> anyhow::Result<()> {
        let ran = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&ran);
        let handle = ctx.spawn(async move {
            marker.store(true, Ordering::SeqCst);
            42_u32
        });
        let value = handle.await?;
        if value != 42_u32 {
            anyhow::bail!("unexpected spawn result: {value}");
        }
        if !ran.load(Ordering::SeqCst) {
            anyhow::bail!("spawn body never ran");
        }
        Ok(())
    }

    #[rudzio::test]
    fn cancel_token_is_child(ctx: &Test) -> anyhow::Result<()> {
        if ctx.cancel_token().is_cancelled() {
            anyhow::bail!("per-test cancel token was already cancelled");
        }
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
