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
