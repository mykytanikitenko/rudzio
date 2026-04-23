//! Tokio-backed [`Runtime`](crate::runtime::Runtime) implementations.
//!
//! Each concrete runtime is gated behind its own feature so consumers
//! only pay for the ones they use. Every variant still requires the
//! shared error-conversion helpers in [`error`], which in turn need
//! `tokio::task::JoinError`.

/// Current-thread tokio runtime with a `LocalSet` for `!Send` futures.
#[cfg(feature = "runtime-tokio-current-thread")]
mod current_thread;
/// Conversion helpers for tokio's task errors. Always compiled when any
/// tokio-backed runtime is on; `use tokio::task::JoinError` is
/// available from the bare `tokio` dep (base `rt` feature).
pub(crate) mod error;
/// Tokio `LocalRuntime` — single-thread runtime with native `!Send` support.
#[cfg(feature = "runtime-tokio-local")]
mod local;
/// Multi-thread tokio runtime.
#[cfg(feature = "runtime-tokio-multi-thread")]
mod multi_thread;

#[cfg(feature = "runtime-tokio-current-thread")]
pub use current_thread::CurrentThread;
#[cfg(feature = "runtime-tokio-local")]
pub use local::Local;
#[cfg(feature = "runtime-tokio-multi-thread")]
pub use multi_thread::Multithread;
