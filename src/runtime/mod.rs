//! Async runtime abstraction and concrete implementations.

/// Error type returned when a spawned task fails to join.
mod join_error;
/// The [`Runtime`] trait defining the async runtime abstraction.
mod rt;

#[cfg(feature = "runtime-compio")]
pub mod compio;
#[cfg(feature = "runtime-embassy")]
pub mod embassy;
#[cfg(feature = "runtime-tokio")]
pub mod tokio;

pub use join_error::JoinError;
pub use rt::Runtime;
