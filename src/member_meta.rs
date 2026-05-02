//! Per-member manifest-dir registry used by [`crate::manifest_dir!`].
//!
//! Member integration tests (`tests/*.rs`) get `#[path]`-included into
//! the cargo-rudzio aggregator. `env!("CARGO_MANIFEST_DIR")` at the
//! include site resolves to the aggregator's manifest dir, not the
//! original member's — so harnesses that locate fixtures relative to
//! the member's dir can't find them under `cargo rudzio test`.
//!
//! The aggregator's `build.rs` exports
//! `cargo:rustc-env=RUDZIO_MEMBER_MANIFEST_DIR_<sanitised>=<abs path>`
//! for each member, and the aggregator's `main.rs` calls
//! [`register_member_manifest_dirs`] with the resolved values at
//! startup. `module_path!()` at the test's call site identifies which
//! member's dir to return.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Marker module name used by the cargo-rudzio aggregator's
/// `src/main.rs` to host every member's `#[path]`-included tests.
///
/// Has to be unique enough that it can't collide with a `mod tests`
/// inside a bridged member's own src tree — otherwise the parser
/// below would mistake an internal test mod for the aggregator's
/// outer `tests` mod and lift the wrong segment.
const AGGREGATOR_TESTS_MARKER: &str = "__rudzio_member_tests";

/// Registry populated once at aggregator startup.
///
/// Maps the sanitised member name (cargo's `[package].name` with `-`
/// replaced by `_`, matching the aggregator's `mod <member>` blocks) to
/// the member's original manifest dir.
static REGISTRY: OnceLock<HashMap<&'static str, PathBuf>> = OnceLock::new();

/// Install the per-member manifest-dir map.
///
/// Called once from the cargo-rudzio aggregator's `main.rs` body before
/// the runner starts. Subsequent calls are silently ignored — the
/// registry is intentionally write-once so test code observes a stable
/// view regardless of when it executes.
#[doc(hidden)]
#[inline]
pub fn register_member_manifest_dirs(entries: &[(&'static str, &'static str)]) {
    let map: HashMap<&'static str, PathBuf> = entries
        .iter()
        .map(|(member, dir)| (*member, PathBuf::from(*dir)))
        .collect();
    let _ignored: Result<(), HashMap<&'static str, PathBuf>> = REGISTRY.set(map);
}

/// Resolve the manifest dir for the test crate that owns the call site.
///
/// `module_path` is the value of [`std::module_path!`] at the call site.
/// Under the cargo-rudzio aggregator it has the shape
/// `<aggregator_crate>::tests::<member>::...`; the function lifts the
/// segment after `tests` and looks it up in the registry. When the
/// registry is empty (per-crate `cargo test` builds, or `module_path`
/// doesn't have a `tests` segment), `fallback` is returned — typically
/// the call site's own `env!("CARGO_MANIFEST_DIR")`, which is correct
/// outside the aggregator.
#[doc(hidden)]
#[inline]
#[must_use]
pub fn resolve_member_manifest_dir(module_path: &str, fallback: &str) -> PathBuf {
    parse_member_segment(module_path)
        .and_then(|name| REGISTRY.get().and_then(|map| map.get(name)).cloned())
        .unwrap_or_else(|| PathBuf::from(fallback))
}

/// Lift the member name out of an aggregator-style `module_path!()`.
///
/// Returns `None` for module paths that don't contain the aggregator's
/// marker segment (per-crate test binaries, src code, etc.) so the
/// resolver falls back to the call site's `CARGO_MANIFEST_DIR`.
#[doc(hidden)]
#[inline]
#[must_use]
pub fn parse_member_segment(module_path: &str) -> Option<&str> {
    module_path
        .split("::")
        .skip_while(|segment| *segment != AGGREGATOR_TESTS_MARKER)
        .nth(1)
}
