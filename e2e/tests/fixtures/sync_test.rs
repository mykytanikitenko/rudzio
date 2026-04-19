//! Compile-only assertion that the suite macro accepts a sync test body.

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
    use super::*;

    #[rudzio::test]
    fn sync_passes(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
