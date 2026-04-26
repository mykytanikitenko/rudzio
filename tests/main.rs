//! Per-crate test binary for rudzio. The tokens come from `runner.rs`
//! below; this file is just the `#[rudzio::main]` entry.

mod parallelism_tests;
mod phase_wrapper;
mod render_idle_redraw;
mod runner;

#[rudzio::main]
fn main() {}
