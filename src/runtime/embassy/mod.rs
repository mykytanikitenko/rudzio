//! Embassy runtime backed by `embassy-executor` with `arch-std`.
//!
//! The executor runs on a dedicated background OS thread (because
//! `Executor::run()` is `-> !`). Tests and framework futures are submitted
//! as embassy tasks via a `Spawner` and results are communicated back through
//! `std::sync::mpsc` channels.

mod runtime;

pub use runtime::Runtime;

/// Create a new embassy runtime instance.
///
/// # Errors
///
/// Returns an error if the background executor thread cannot be started.
#[inline]
pub fn new() -> std::io::Result<Runtime> {
    Runtime::new()
}
