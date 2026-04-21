//! Workspace-aggregated `#[rudzio::main]` binary.
//!
//! Pulls every testable crate's integration-test source file into this
//! binary via `#[path]`, so every `#[rudzio::test]` token registered
//! through `linkme` lands in one `TEST_TOKENS` slice and one runner
//! schedules them all together. `rudzio::build::expose_bins` (in
//! `build.rs`) populates `CARGO_BIN_EXE_<n>` for the `rudzio-fixtures`
//! bin targets so the aggregated integration file's `env!(...)` calls
//! resolve at compile time.

// `linkme::distributed_slice` expansion emits statics with
// `#[link_section]`, which rustc classifies as `unsafe_code`. The tests
// below rely on that mechanism via `#[rudzio::suite]`.
#![allow(
    unsafe_code,
    reason = "linkme's distributed_slice mechanism requires #[link_section]"
)]

mod tests;

#[rudzio::main]
fn main() {}
