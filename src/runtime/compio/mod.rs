//! Compio-backed [`Runtime`](crate::runtime::Runtime) implementation.

/// Compio runtime wrapper implementing the [`Runtime`](crate::runtime::Runtime) trait.
mod runtime;

pub use runtime::Runtime;
pub use runtime::new;
