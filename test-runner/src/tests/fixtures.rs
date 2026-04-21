//! Pull `rudzio-fixtures`'s integration-test source into this binary via
//! `#[path]`. The file's `env!("CARGO_BIN_EXE_<name>")` calls resolve
//! because `build.rs` called `rudzio_build::expose_bins("rudzio-fixtures")`
//! before rustc started compiling this source.
//!
//! `tests/compile.rs` (the trybuild harness) is intentionally skipped:
//! its `cases.pass("tests/fixtures/sync_test.rs")` is manifest-dir
//! relative and that path doesn't exist from this crate's perspective.
//! Per-crate `cargo test -p rudzio-fixtures` still runs it.

#[path = "../../../fixtures/tests/integration.rs"]
mod integration;
