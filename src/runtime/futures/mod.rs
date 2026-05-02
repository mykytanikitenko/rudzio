//! Runtime built on top of [`futures::executor::ThreadPool`].
//!
//! `spawn` / `spawn_blocking` run on the shared `ThreadPool`; `spawn_local`
//! queues onto a per-runtime `LocalPool` that `block_on` drives alongside
//! the main future via `LocalPool::run_until`. `sleep` uses
//! [`futures_timer::Delay`].

/// Internal wiring for the `futures::executor::ThreadPool` backend.
mod runtime;

pub use runtime::ThreadPool;
