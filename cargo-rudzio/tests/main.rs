//! `cargo-rudzio` integration test entry point.
//!
//! Aggregates the `generate_spec` and `spawn_env_spec` test modules under
//! a single `#[rudzio::main]` per the workspace-wide one-binary rule.

mod args_spec;
mod cross_bridge_spec;
mod generate_spec;
mod spawn_env_spec;

#[rudzio::main]
fn main() {}
