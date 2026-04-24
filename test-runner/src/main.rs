//! Workspace-aggregated `#[rudzio::main]` binary.
//!
//! This binary links against every testable crate in the workspace whose
//! tests live in lib code — specifically `rudzio` (via its `tests`
//! feature) — and additionally recompiles the `rudzio-macro-internals`
//! integration tests into this binary via `#[path]` includes (they can't
//! live in lib code because macro-internals would need a feature-gated
//! dep on `rudzio` which Cargo rejects as a cycle).
//!
//! `rudzio-e2e`'s tests stay per-crate because they reference
//! `env!("CARGO_BIN_EXE_...")` which is only set for integration tests of
//! the crate defining those binaries.

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
