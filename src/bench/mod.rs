//! Benchmarking instrument.
//!
//! A test annotated with `#[rudzio::test(benchmark = <strategy>)]` runs the
//! body multiple times under the given [`Strategy`] when the runner is
//! invoked with `--bench`. Without `--bench`, the body runs exactly once as
//! a smoke test — the bench annotation is a no-op, so every bench test is
//! also a valid regular test without changing anything.
//!
//! The strategy interface is a single [`Strategy::run`] method that takes a
//! closure producing a fresh future per call and returns a [`Report`]
//! aggregating per-iteration timings plus failure and panic counts. Two
//! primitive strategies ship with rudzio: [`strategy::Sequential`] (N
//! one-after-another iterations) and [`strategy::Concurrent`] (N
//! `join_all`-driven concurrent futures on the same task). Custom
//! strategies can be written by implementing [`Strategy`] directly — the
//! trait is intentionally minimal so composition (run A then B, repeat K
//! rounds, etc.) is just a matter of writing a new impl.

pub mod dist_summary;
pub mod progress_snapshot;
pub mod report;
pub mod strategy;

pub use dist_summary::DistSummary;
pub use progress_snapshot::{HISTOGRAM_BUCKETS, ProgressSnapshot};
pub use report::Report;
pub use strategy::Strategy;
