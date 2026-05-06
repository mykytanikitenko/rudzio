//! Runtime built on top of [`smol::Executor`].
//!
//! Owns an [`smol::Executor`] driven by a fixed pool of OS worker threads
//! plus a per-runtime [`smol::LocalExecutor`] for `!Send` futures. `block_on`
//! runs the user future on the local executor while the worker threads keep
//! the global executor making progress. `spawn` queues onto the global
//! executor, `spawn_local` onto the local one, both wrapped with
//! `AssertUnwindSafe(_).catch_unwind()` to convert task panics into
//! [`JoinError::Panicked`]. `spawn_blocking` defers to [`smol::unblock`].
//! `sleep` uses [`smol::Timer`].
//!
//! Worker threads run `smol::block_on(executor.run(shutdown_rx.recv()))` and
//! exit when the shutdown sender is dropped on `Runtime::drop`.

/// Internal wiring for the `smol` backend.
mod runtime;

pub use runtime::Runtime;
