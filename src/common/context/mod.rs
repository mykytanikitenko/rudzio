//! Common [`Suite`] and [`Test`] context implementations with a shared
//! [`CancellationToken`](tokio_util::sync::CancellationToken) and
//! [`TaskTracker`](tokio_util::task::TaskTracker).

/// Shared-state suite context impl.
mod suite;
/// Per-test context impl built on top of the suite context.
mod test;

pub use suite::Suite;
pub use test::Test;
