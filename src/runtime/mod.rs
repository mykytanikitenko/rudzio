//! Async runtime abstraction and concrete implementations.

/// Error type returned when a spawned task fails to join.
mod join_error;
/// The [`Runtime`] trait defining the async runtime abstraction.
mod rt;

#[cfg(feature = "runtime-async-std")]
pub mod async_std;
#[cfg(feature = "runtime-compio")]
pub mod compio;
#[cfg(feature = "runtime-embassy")]
pub mod embassy;
#[cfg(feature = "runtime-futures")]
pub mod futures;
#[cfg(feature = "runtime-smol")]
pub mod smol;
#[cfg(any(
    feature = "runtime-tokio-multi-thread",
    feature = "runtime-tokio-current-thread",
    feature = "runtime-tokio-local",
))]
pub mod tokio;

pub use join_error::JoinError;
pub use rt::Runtime;
