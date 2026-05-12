//! Runtime built on top of [`async_std::task`].
//!
//! `block_on` defers to [`async_std::task::block_on`]; `spawn` /
//! `spawn_local` / `spawn_blocking` map to the matching `async_std::task`
//! entry points, with `AssertUnwindSafe(_).catch_unwind()` wrappers that
//! convert task panics into [`JoinError::Panicked`] instead of letting them
//! propagate through the awaiter. `sleep` uses [`async_std::task::sleep`].

/// Internal wiring for the `async-std` backend.
mod runtime;

pub use runtime::Runtime;
