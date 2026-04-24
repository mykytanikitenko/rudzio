//! Library surface for the `cargo-rudzio` subcommand. Exposes the
//! aggregator-generation pipeline so integration tests can drive it
//! against synthetic inputs.

#![allow(
    unused_results,
    clippy::needless_pass_by_value,
    reason = "toml_edit's insert/push API routinely returns the previous value; CLI glue does not care about the dropped option"
)]

pub mod generate;

/// The cfg symbol that `cargo rudzio test` activates so unit-test
/// modules guarded with `#[cfg(any(test, rudzio_test))]` compile into
/// the aggregator build. See `README.md` § Running tests.
pub const RUDZIO_TEST_CFG_FLAG: &str = "--cfg";
pub const RUDZIO_TEST_CFG_VALUE: &str = "rudzio_test";

/// Sentinel env var name read by `rudzio::build::expose_bins` to detect
/// re-entry. Kept in sync with the constant of the same value in
/// `rudzio/src/build.rs`. When `cargo rudzio test` spawns cargo, it
/// sets this env var so that any bridge crate forwarding a member's
/// `build.rs` (which may call `rudzio::build::expose_self_bins()`)
/// short-circuits to Ok instead of trying to build bins on the bridge's
/// own package (which has no `[[bin]]` targets). The aggregator does
/// its own bin discovery from its generated build.rs, so nested
/// expose_bins calls would be redundant anyway.
pub const EXPOSE_BINS_SENTINEL_ENV: &str = "__RUDZIO_EXPOSE_BINS_ACTIVE";
pub const EXPOSE_BINS_SENTINEL_VALUE: &str = "1";

/// Compute the `RUSTFLAGS` value to set when spawning cargo for the
/// `test` subcommand. Appends `--cfg rudzio_test` to any existing
/// flags, preserving them; no-op if the flag is already present.
#[must_use]
pub fn resolve_rustflags(existing: Option<&str>) -> String {
    let trimmed = existing.unwrap_or("").trim();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let already_present = tokens
        .windows(2)
        .any(|w| w[0] == RUDZIO_TEST_CFG_FLAG && w[1] == RUDZIO_TEST_CFG_VALUE);
    if already_present {
        trimmed.to_owned()
    } else if trimmed.is_empty() {
        format!("{RUDZIO_TEST_CFG_FLAG} {RUDZIO_TEST_CFG_VALUE}")
    } else {
        format!("{trimmed} {RUDZIO_TEST_CFG_FLAG} {RUDZIO_TEST_CFG_VALUE}")
    }
}

/// Env vars to set when spawning cargo for the aggregator build inside
/// `cargo rudzio test`. Returned as `(name, value)` pairs so callers
/// (and tests) can inspect the full set without invoking cargo.
///
/// Currently: `RUSTFLAGS` with `--cfg rudzio_test` appended, plus the
/// expose-bins re-entry sentinel that short-circuits nested
/// `rudzio::build::expose_bins` calls in bridge-forwarded build
/// scripts.
#[must_use]
pub fn spawn_env(existing_rustflags: Option<&str>) -> Vec<(&'static str, String)> {
    vec![
        ("RUSTFLAGS", resolve_rustflags(existing_rustflags)),
        (
            EXPOSE_BINS_SENTINEL_ENV,
            EXPOSE_BINS_SENTINEL_VALUE.to_owned(),
        ),
    ]
}
