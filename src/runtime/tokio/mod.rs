//! Tokio-backed [`Runtime`](crate::runtime::Runtime) implementations.

/// Current-thread tokio runtime with a `LocalSet` for `!Send` futures.
mod current_thread;
/// Conversion helpers for tokio's task errors.
pub(crate) mod error;
/// Tokio `LocalRuntime` — single-thread runtime with native `!Send` support.
mod local;
/// Multi-thread tokio runtime.
mod multi_thread;

pub use current_thread::CurrentThread;
pub use local::Local;
pub use multi_thread::Multithread;
