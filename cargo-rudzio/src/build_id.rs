//! Compile-time build identity, stamped by `build.rs` and reported via
//! `cargo rudzio --build-id`.
//!
//! # Why
//!
//! `cargo-rudzio`'s e2e specs spawn the binary through
//! `env!("CARGO_BIN_EXE_cargo-rudzio")` — a path baked into the test
//! binary at COMPILE time and resolved against the ambient
//! `CARGO_TARGET_DIR`. When that target dir is shared across agents at
//! different commits, the path can name a binary built from a different
//! HEAD than the test source; a fix then "passes green" against a binary
//! that never carried the fix. Stamping the git identity into the binary
//! and re-checking it before every spawn (see
//! `cargo-rudzio-test-helpers::assert_fresh_binary`) makes that class of
//! false green structurally impossible.

/// Full git HEAD SHA the binary was built from, or `"unknown"` when the
/// build ran outside a git checkout or without a `git` binary on PATH.
pub const GIT_HEAD: &str = match option_env!("CARGO_RUDZIO_BUILD_GIT_HEAD") {
    Some(value) => value,
    None => "unknown",
};

/// Working-tree state at build time: `"clean"`, `"dirty"`, or
/// `"unknown"` (git unavailable / not a repository).
pub const GIT_DIRTY: &str = match option_env!("CARGO_RUDZIO_BUILD_GIT_DIRTY") {
    Some(value) => value,
    None => "unknown",
};

/// Crate version stamped by `build.rs` from the real package's
/// `CARGO_PKG_VERSION`.
///
/// A bridge compiles this source under a synthesized package whose own
/// `CARGO_PKG_VERSION` is `0.0.0`, so the value is taken from the build
/// script, which sees the real member's version.
pub const VERSION: &str = env!("CARGO_RUDZIO_BUILD_VERSION");

/// `<version>-<unix-epoch-seconds>` build stamp emitted by `build.rs`.
///
/// Distinct builds produce distinct stamps; the aggregator subdir name
/// hashes this so a rebuilt `cargo-rudzio` forces a fresh consumer target
/// dir.
pub const BUILD_STAMP: &str = env!("CARGO_RUDZIO_BUILD_STAMP");

/// Cargo profile the binary was compiled under (`debug`, `release`, …).
pub const PROFILE: &str = match option_env!("CARGO_RUDZIO_BUILD_PROFILE") {
    Some(value) => value,
    None => "unknown",
};

/// Content fingerprint of this crate's own build inputs.
///
/// Covers `src/`, `build.rs`, and `Cargo.toml`, stamped by `build.rs` and
/// compared against the live sources by
/// `cargo-rudzio-test-helpers::assert_fresh_binary`. Unlike `GIT_HEAD`, this
/// is invariant under a HEAD move that does not touch the sources, so a
/// commit of empty harness/doco commits does not falsely mark the binary
/// stale.
pub const SOURCE_HASH: &str = match option_env!("CARGO_RUDZIO_BUILD_SOURCE_HASH") {
    Some(value) => value,
    None => "unknown",
};

/// Single-line, space-separated `key=value` identity report.
///
/// The `git-head=` and `git-dirty=` token shapes are a cross-crate
/// contract with `cargo-rudzio-test-helpers::assert_fresh_binary`, which
/// parses this line. Keep them stable.
#[inline]
#[must_use]
pub fn line() -> String {
    format!(
        "cargo-rudzio-build-id git-head={GIT_HEAD} git-dirty={GIT_DIRTY} version={VERSION} build-stamp={BUILD_STAMP} profile={PROFILE} source-hash={SOURCE_HASH}"
    )
}
