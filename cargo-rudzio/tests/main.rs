//! `cargo-rudzio` integration test entry point.
//!
//! Aggregates the `generate_spec` and `spawn_env_spec` test modules under
//! a single `#[rudzio::main]` per the workspace-wide one-binary rule.

mod bug_10_helper_isolation_edge_cases_spec;
mod bug_11_double_execution_edge_cases_spec;
mod bug_12_bare_use_alias_spec;
mod bug_14_monolith_sigkill_spec;
mod bug_16_bin_source_tests_invisible_spec;
mod bug_18_aggregator_cache_invalidation_spec;
mod bug_19_bin_crate_root_resolution_spec;
mod bug_8_helper_discovery_edge_cases_spec;
mod cfg_test_warning_narrowing_spec;
mod cli_spec;
mod crate_path_under_aggregator_spec;
mod cross_bridge_spec;
mod features_table_propagation_spec;
mod generate_spec;
mod main_rs_test_root_helper_spec;
mod migration_bug_02_lib_harness_false_clippy_spec;
mod migration_bug_03_coverage_profraw_spec;
mod migration_bug_07_extern_crate_self_collision_spec;
mod migration_bug_08_bin_only_crates_spec;
mod optional_dep_force_enable_spec;
mod package_filter_transitive_spec;
mod parallel_invocations_shared_target_dir_spec;
mod path_attribute_helper_spec;
mod per_binary_helper_isolation_spec;
mod proper_architecture_spec;
mod registry_preservation_spec;
mod silent_member_drop_invariant_spec;
mod strict_parse_spec;
mod surface_visibility_invariant_spec;
mod synth_name_leak_spec;
mod test_filter_spec;

#[rudzio::main]
fn main() {}
