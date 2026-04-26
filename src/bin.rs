//! Resolve a `[[bin]]` target's executable path at test call sites.
//!
//! The primary entry point is the [`crate::bin!`] macro; this module
//! holds the runtime backing that the macro falls back to when cargo
//! hasn't populated `CARGO_BIN_EXE_<name>`.
//!
//! # Why this exists
//!
//! Cargo only sets `CARGO_BIN_EXE_<name>` automatically for integration
//! tests (`tests/*.rs`) of the crate that declares the `[[bin]]`. Two
//! common rudzio layouts fall outside that case:
//!
//! - **Shared runner / aggregator** — a separate crate pulls in tests
//!   from a bin-owning crate to run them under a single
//!   `#[rudzio::main]`.
//! - **`cargo test --lib`** — `#[cfg(test)] #[rudzio::main]` lives in
//!   `src/lib.rs` and is run via `cargo test --lib`. That test binary
//!   is a `--lib` test target, not an integration test, so cargo does
//!   not populate `CARGO_BIN_EXE_<name>` for it.
//!
//! The [`crate::bin!`] macro papers over all three layouts:
//!
//! 1. `option_env!(concat!("CARGO_BIN_EXE_", <name>))` at compile time
//!    — hits for integration tests (cargo populates) and for any crate
//!    whose `build.rs` ran [`crate::build::expose_bins`] /
//!    [`crate::build::expose_self_bins`] (they emit the same env var).
//! 2. Runtime walk from `std::env::current_exe()` up past `deps/` to
//!    `target/<profile>/<bin>` — hits for `cargo test --lib` with a
//!    bin already built by `cargo build --bins`.
//! 3. If neither resolves, panic with a message that tells the user
//!    exactly which fix applies (pre-build, or add `build.rs`).

use std::ffi::OsString;
use std::path::PathBuf;

/// Error reported by the runtime fallback behind the [`crate::bin!`]
/// macro. Exposed so the macro can render its `Display` form in a
/// `panic!` — users do not construct this directly.
#[derive(Debug)]
#[doc(hidden)]
pub struct BinNotFound {
    message: String,
}

impl std::fmt::Display for BinNotFound {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BinNotFound {}

/// Runtime backing for the [`crate::bin!`] macro. Only called when
/// `option_env!("CARGO_BIN_EXE_<name>")` was `None` at compile time.
///
/// Cargo places a test binary at `<target>/<profile>/deps/<name>-<hash>`,
/// so two `.parent()` hops land in `<target>/<profile>/`, where any
/// `cargo build --bins` output also lives. This covers the
/// `cargo test --lib` case where no build script has set
/// `CARGO_BIN_EXE_<name>` but the bin has been built manually.
///
/// # Errors
///
/// Returns [`BinNotFound`] if `current_exe()` fails, the expected
/// ancestor directories are missing, or the bin file isn't on disk.
#[doc(hidden)]
#[inline]
pub fn __resolve_at_runtime(bin_name: &str) -> Result<PathBuf, BinNotFound> {
    let current = std::env::current_exe().map_err(|e| BinNotFound {
        message: format!(
            "rudzio::bin!(\"{bin_name}\"): failed to read \
             `std::env::current_exe()`: {e}. Cargo's test runner normally \
             makes this reliable, so something unusual is going on with the \
             process environment."
        ),
    })?;
    let deps_dir = current.parent().ok_or_else(|| BinNotFound {
        message: format!(
            "rudzio::bin!(\"{bin_name}\"): the current exe `{}` has no \
             parent directory, so the runtime fallback can't locate a \
             `target/<profile>/` to search.",
            current.display()
        ),
    })?;
    let profile_dir = deps_dir.parent().ok_or_else(|| BinNotFound {
        message: format!(
            "rudzio::bin!(\"{bin_name}\"): the deps directory `{}` has no \
             parent, so the runtime fallback can't walk up to a \
             `target/<profile>/` to search.",
            deps_dir.display()
        ),
    })?;
    let file_name: OsString = if cfg!(windows) {
        OsString::from(format!("{bin_name}.exe"))
    } else {
        OsString::from(bin_name)
    };
    let candidate = profile_dir.join(&file_name);
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(BinNotFound {
        message: format!(
            "rudzio::bin!(\"{bin_name}\"): no binary at `{}`. \
             `CARGO_BIN_EXE_{bin_name}` wasn't set at compile time and the \
             bin isn't built in the expected target subdirectory. Either \
             run `cargo build --bins` before the tests, or add a \
             `build.rs` to this crate with \
             `rudzio::build::expose_self_bins()?` so the bin is produced \
             (and wired up via `CARGO_BIN_EXE_*`) as part of the \
             test-binary build.",
            candidate.display()
        ),
    })
}
