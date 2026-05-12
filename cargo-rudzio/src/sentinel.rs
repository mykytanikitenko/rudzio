//! Recursion-guard sentinel env vars carried across `cargo rudzio test`
//! spawns so nested `expose_bins` calls don't recursively re-enter
//! `cargo build --bins`.

/// Sentinel env var name read by `rudzio::build::expose_bins` to detect re-entry.
///
/// Kept in sync with the constant of the same value in
/// `rudzio/src/build.rs`. When `cargo rudzio test` spawns cargo, it
/// sets this env var so that any nested `expose_bins` call (e.g. from a
/// member's own `build.rs` running under the aggregator chain)
/// short-circuits instead of recursing into another `cargo build --bins`
/// — which would otherwise accumulate nested `rudzio-bin-cache/...`
/// target dirs indefinitely.
pub const EXPOSE_BINS_SENTINEL_ENV: &str = "__RUDZIO_EXPOSE_BINS_ACTIVE";
/// Value the [`EXPOSE_BINS_SENTINEL_ENV`] env var is set to when a
/// `cargo rudzio test` spawn is in progress.
pub const EXPOSE_BINS_SENTINEL_VALUE: &str = "1";

/// Env vars to set when spawning cargo for the aggregator build inside
/// `cargo rudzio test`. Returned as `(name, value)` pairs so callers
/// (and tests) can inspect the full set without invoking cargo.
///
/// The `--cfg rudzio_test` activation is **not** transported via
/// ambient `RUSTFLAGS` — that approach leaked the cfg into nested
/// `cargo build --bins` invocations spawned by `expose_bins`, blowing
/// up the build with thousands of unresolved-crate errors. Each
/// compile unit that needs the cfg (the aggregator and every bridge)
/// emits `cargo:rustc-cfg=rudzio_test` from its own `build.rs` — see
/// `generate::build_build_rs` and `generate::build_bridge_build_rs`.
#[inline]
#[must_use]
pub fn spawn_env() -> Vec<(&'static str, String)> {
    vec![(
        EXPOSE_BINS_SENTINEL_ENV,
        EXPOSE_BINS_SENTINEL_VALUE.to_owned(),
    )]
}
