//! `cargo-rudzio` integration test entry point.
//!
//! Aggregates the `generate_spec` and `spawn_env_spec` test modules under
//! a single `#[rudzio::main]` per the workspace-wide one-binary rule.

mod build_dir_deadlock_spec;
mod cli_spec;
mod crate_path_under_aggregator_spec;
mod cross_bridge_spec;
mod generate_spec;
mod mod_rs_codegen_spec;
mod optional_dep_force_enable_spec;
mod registry_preservation_spec;
mod spawn_env_spec;
mod strict_parse_spec;

#[rudzio::main]
fn main() {}
