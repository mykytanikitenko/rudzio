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

/// Crate version, mirroring `CARGO_PKG_VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `<version>-<unix-epoch-seconds>` stamp emitted by `build.rs`.
///
/// Distinct builds produce distinct stamps; the aggregator subdir name
/// hashes this so a rebuilt `cargo-rudzio` forces a fresh consumer
/// target dir.
pub const BUILD_STAMP: &str = env!("CARGO_RUDZIO_BUILD_STAMP");

/// Cargo profile the binary was compiled under (`debug`, `release`, …).
pub const PROFILE: &str = match option_env!("CARGO_RUDZIO_BUILD_PROFILE") {
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
        "cargo-rudzio-build-id git-head={GIT_HEAD} git-dirty={GIT_DIRTY} version={VERSION} build-stamp={BUILD_STAMP} profile={PROFILE}"
    )
}
