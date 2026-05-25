//! `cargo-rudzio` integration test entry point.
//!
//! Aggregates the `generate_spec` and `spawn_env_spec` test modules under
//! a single `#[rudzio::main]` per the workspace-wide one-binary rule.

mod bug_10_helper_isolation_edge_cases_spec;
mod bug_11_double_execution_edge_cases_spec;
mod bug_12_bare_use_alias_spec;
mod bug_14_monolith_sigkill_spec;
mod bug_8_helper_discovery_edge_cases_spec;
mod build_dir_deadlock_spec;
mod cfg_test_warning_narrowing_spec;
mod cli_spec;
mod crate_path_under_aggregator_spec;
mod cross_bridge_spec;
mod double_execution_spec;
mod features_table_propagation_spec;
mod generate_spec;
mod main_rs_test_root_helper_spec;
mod mod_rs_codegen_spec;
mod optional_dep_force_enable_spec;
mod package_filter_transitive_spec;
mod parallel_invocations_shared_target_dir_spec;
mod path_attribute_helper_spec;
mod per_binary_helper_isolation_spec;
mod registry_preservation_spec;
mod spawn_env_spec;
mod strict_parse_spec;
mod synth_name_leak_spec;
mod test_filter_spec;

#[rudzio::main]
fn main() {}
