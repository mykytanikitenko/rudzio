//! Monoio-backed [`Runtime`](crate::runtime::Runtime) implementation.

/// Monoio runtime wrapper implementing the [`Runtime`](crate::runtime::Runtime) trait.
mod runtime;

pub use runtime::Runtime;
pub use runtime::new;
