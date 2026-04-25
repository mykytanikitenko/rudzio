//! Per-crate test binary for rudzio. The tokens come from `runner.rs`
//! below; this file is just the `#[rudzio::main]` entry.

mod phase_wrapper;
mod runner;

#[rudzio::main]
fn main() {}
