//! Runtime built on top of [`futures::executor::ThreadPool`].
//!
//! `spawn` / `spawn_blocking` run on the shared `ThreadPool`; `spawn_local`
//! queues onto a per-runtime `LocalPool` that `block_on` drives alongside
//! the main future via `LocalPool::run_until`. `sleep` uses
//! [`futures_timer::Delay`].

mod runtime;

pub use runtime::ThreadPool;

/// Create a new `futures::executor::ThreadPool`-backed runtime.
///
/// # Errors
///
/// Returns an error if the underlying `ThreadPool` cannot be built.
#[inline]
pub fn new(config: &crate::config::Config) -> std::io::Result<ThreadPool> {
    ThreadPool::new(config)
}
