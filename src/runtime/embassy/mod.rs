//! Embassy runtime backed by `embassy-executor` with `arch-std`.
//!
//! The executor runs on a dedicated background OS thread (because
//! `Executor::run()` is `-> !`). Tests and framework futures are submitted
//! as embassy tasks via a `Spawner` and results are communicated back through
//! `std::sync::mpsc` channels.

/// Embassy backend wired to a dedicated thread.
mod runtime;

pub use runtime::Runtime;
