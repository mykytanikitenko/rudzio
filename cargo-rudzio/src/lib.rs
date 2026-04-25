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

/// Inverse of [`resolve_rustflags`]: remove every `--cfg rudzio_test`
/// pair from an existing `RUSTFLAGS` string, preserving other tokens.
///
/// Used by the generated `build.rs` scripts (both the aggregator's and
/// bridge-local ones) before shelling out to nested `cargo build --bins`
/// for member bins. Without this, the ambient `RUSTFLAGS=--cfg rudzio_test`
/// that `cargo rudzio test` sets would activate `cfg(any(test,
/// rudzio_test))`-gated modules during bin compilation — and those
/// modules typically reference dev-deps that aren't in the bin's
/// `[dependencies]`, producing hundreds of spurious compile errors.
#[must_use]
pub fn strip_rudzio_test_cfg(rustflags: &str) -> String {
    let tokens: Vec<&str> = rustflags.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == RUDZIO_TEST_CFG_FLAG
            && tokens.get(i + 1).copied() == Some(RUDZIO_TEST_CFG_VALUE)
        {
            i += 2;
            continue;
        }
        out.push(tokens[i]);
        i += 1;
    }
    out.join(" ")
}

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
