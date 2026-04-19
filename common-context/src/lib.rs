//! Common [`Global`] and [`Test`] context implementations for rudzio, with a shared
//! [`CancellationToken`](tokio_util::sync::CancellationToken) and
//! [`TaskTracker`](tokio_util::task::TaskTracker).

/// Shared global context implementation.
mod global;
/// Shared per-test context implementation.
mod test_context;

pub use global::Global;
pub use test_context::Test;
