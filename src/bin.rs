//! Resolve a `[[bin]]` target's executable path at test call sites.
//!
//! The primary entry point is the [`crate::bin!`] macro; this module
//! also holds the runtime backing that the macro falls back to when
//! cargo hasn't populated `CARGO_BIN_EXE_<name>`.
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

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// Resolve a `[[bin]]` target's executable path at a test call site,
/// returning a [`PathBuf`](std::path::PathBuf).
///
/// Drop-in replacement for `PathBuf::from(env!("CARGO_BIN_EXE_<name>"))`
/// that also works in the two layouts cargo doesn't populate
/// `CARGO_BIN_EXE_*` for on its own:
///
/// - **Shared runner** — an aggregator crate hosting tests that spawn
///   another crate's bins. Requires [`crate::build::expose_bins`] in
///   the aggregator's `build.rs`.
/// - **`cargo test --lib`** — `#[cfg(test)] #[rudzio::main]` in
///   `src/lib.rs`. Either add [`crate::build::expose_self_bins`] to the
///   crate's `build.rs`, or pre-build with `cargo build --bins`.
///
/// Resolution chain:
///
/// 1. `option_env!(concat!("CARGO_BIN_EXE_", <name>))` at compile time.
/// 2. Runtime walk from `std::env::current_exe()` to
///    `target/<profile>/<name>` if step 1 missed.
/// 3. Panic with an actionable message (which fix to apply) if both
///    miss.
///
/// ```rust,ignore
/// let mut child = std::process::Command::new(rudzio::bin!("my-server"))
///     .arg("--port=0")
///     .spawn()?;
/// ```
///
/// The argument must be a string literal (the bin's Cargo target name).
#[macro_export]
macro_rules! bin {
    ($name:literal) => {{
        match ::core::option_env!(::core::concat!("CARGO_BIN_EXE_", $name)) {
            ::core::option::Option::Some(path) => ::std::path::PathBuf::from(path),
            ::core::option::Option::None => match $crate::bin::__resolve_at_runtime($name) {
                ::core::result::Result::Ok(path) => path,
                ::core::result::Result::Err(err) => ::core::panic!("{err}"),
            },
        }
    }};
}

/// Error reported by the runtime fallback behind the [`crate::bin!`]
/// macro. Exposed so the macro can render its `Display` form in a
/// `panic!` — users do not construct this directly.
#[derive(Debug)]
#[doc(hidden)]
pub struct NotFound {
    message: String,
}

impl fmt::Display for NotFound {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for NotFound {}

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
/// Returns [`NotFound`] if `current_exe()` fails, the expected
/// ancestor directories are missing, or the bin file isn't on disk.
#[doc(hidden)]
#[inline]
pub fn __resolve_at_runtime(bin_name: &str) -> Result<PathBuf, NotFound> {
    let current = env::current_exe().map_err(|err| NotFound {
        message: format!(
            "rudzio::bin!(\"{bin_name}\"): failed to read \
             `std::env::current_exe()`: {err}. Cargo's test runner normally \
             makes this reliable, so something unusual is going on with the \
             process environment."
        ),
    })?;
    let deps_dir = current.parent().ok_or_else(|| NotFound {
        message: format!(
            "rudzio::bin!(\"{bin_name}\"): the current exe `{}` has no \
             parent directory, so the runtime fallback can't locate a \
             `target/<profile>/` to search.",
            current.display()
        ),
    })?;
    let profile_dir = deps_dir.parent().ok_or_else(|| NotFound {
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
    Err(NotFound {
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
