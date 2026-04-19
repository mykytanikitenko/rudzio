//! Reproduces the sync-test bug: the macro's `spawn_blocking` arm produces
//! code that does not compile. The fixture should be accepted by the macro
//! but isn't, because the generated main:
//!   - moves `test_ctx` into `spawn_blocking` and then uses it again for
//!     teardown (use of moved value);
//!   - requires `test_ctx: 'static` for `spawn_blocking`, which it isn't;
//!   - types `spawn_blocking(...).await` as `anyhow::Result<()>` when the
//!     real type is `Result<anyhow::Result<()>, rudzio::JoinError>`.
//!
//! Verified via `trybuild::TestCases::pass(...)` in tests/compile.rs —
//! the `pass` assertion fails today.

use common_context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::main([
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
