//! Build-script helper: make `[[bin]]` targets reachable from
//! `env!("CARGO_BIN_EXE_<name>")` in the calling crate's compiled
//! sources, in the two cases where cargo does *not* set those env vars
//! automatically:
//!
//! 1. **Cross-crate aggregator** — a workspace-wide test runner that
//!    spawns bins owned by *different* crates.
//! 2. **Same-crate `cargo test --lib` runner** — `#[rudzio::main]` in
//!    `src/lib.rs` (with `#[path]`-included integration/e2e modules)
//!    instead of the `tests/main.rs` integration-test layout.
//!
//! Enabled by the `build` feature — declare rudzio as a build-dep with
//! that feature on.
//!
//! # Cross-crate aggregator
//!
//! Cargo only sets `CARGO_BIN_EXE_<name>` for integration tests of the
//! crate that declares the `[[bin]]`. If you're running a
//! workspace-wide aggregator binary that needs to invoke bins from a
//! *different* crate, either (a) re-declare them as `[[bin]]`s in your
//! aggregator (tedious, and duplicated when the source crate
//! adds/removes bins) or (b) call [`expose_bins`] from a `build.rs`.
//! `cargo rudzio test` already does (b) automatically in the generated
//! aggregator's `build.rs`; this helper is the manual equivalent for
//! hand-rolled aggregators.
//!
//! ```rust,no_run
//! // my-runner/build.rs
//! fn main() -> Result<(), rudzio::build::Error> {
//!     rudzio::build::expose_bins("my-bin-crate")
//! }
//! ```
//!
//! # Same-crate `cargo test --lib` runner
//!
//! `tests/main.rs` runners get `CARGO_BIN_EXE_<name>` for free —
//! integration tests of the crate that declares the `[[bin]]` are
//! exactly the case cargo handles automatically. The `cargo test --lib`
//! pattern (a `#[cfg(test)] #[rudzio::main]` in `src/lib.rs`, with
//! `#[path]`-included integration/e2e modules) does *not* — `--lib`
//! builds only the library target, not the bins, and the `--lib` test
//! binary isn't an integration test in cargo's eyes.
//!
//! For that case, pass the *current* crate's name to [`expose_bins`]
//! from the crate's own `build.rs`:
//!
//! ```rust,no_run
//! // build.rs of a crate that owns both the bins and the lib tests
//! fn main() -> Result<(), rudzio::build::Error> {
//!     rudzio::build::expose_bins("my-crate")
//! }
//! ```
//!
//! `expose_bins` will sandbox a `cargo build --bins -p my-crate` into
//! `$OUT_DIR/rudzio-bin-cache`, then emit
//! `cargo:rustc-env=CARGO_BIN_EXE_<name>=<abs path>` for each so
//! `env!("CARGO_BIN_EXE_<name>")` resolves transparently in the lib's
//! compiled sources.
//!
//! # Recursion guard
//!
//! The same-crate case introduces a re-entry possibility: the nested
//! `cargo build --bins -p my-crate` re-runs `my-crate`'s build script,
//! which calls `expose_bins` again. Without a guard this would loop
//! until the OS killed the process tree — an in-the-wild report caught
//! this only after 8.4 GB of nested
//! `rudzio-bin-cache/.../rudzio-bin-cache/...` target directories had
//! accumulated on disk, because cargo's default output is quiet about
//! repeated rebuilds.
//!
//! [`expose_bins`] defends against this with a sentinel env var
//! (`__RUDZIO_EXPOSE_BINS_ACTIVE`) it sets on every nested cargo it
//! spawns. On entry it inspects the sentinel:
//!
//! - **Sentinel unset** (top-level call): proceed normally.
//! - **Sentinel set, `bin_crate == CARGO_PKG_NAME`** (same-crate
//!   re-entry — exactly what `expose_bins("self-crate")` triggers when
//!   the nested cargo re-runs the build script): return `Ok(())`
//!   silently. The outer call is already driving the build; the inner
//!   has nothing to do.
//! - **Sentinel set, different crate** (potential cross-crate cycle —
//!   crate A's `build.rs` calls `expose_bins("B")`, B's `build.rs`
//!   calls `expose_bins("C")`, C's `build.rs` calls `expose_bins("A")`,
//!   …): emit a `cargo::warning=...` and return `Ok(())`. Stopping the
//!   recursion takes priority over reporting the misconfiguration; the
//!   warning gives the user the signal to investigate.
//!
//! # Requirements for the calling crate
//!
//! - `[build-dependencies]`: `rudzio = { version = "...", default-features = false, features = ["build"] }`.
//! - For the cross-crate case: the bin crate listed under
//!   `[dependencies]` (or `[dev-dependencies]`), so `cargo metadata`
//!   shows it in the workspace view from the aggregator's manifest.
//! - For the same-crate case: no extra deps — the crate is already in
//!   its own metadata view.
//!
//! # What it does, step by step
//!
//! 1. Captures `CARGO_MANIFEST_DIR`, `OUT_DIR`, `PROFILE`, `CARGO`, and
//!    `CARGO_PKG_NAME` from the build-script env (all populated by cargo).
//! 2. Reads the `__RUDZIO_EXPOSE_BINS_ACTIVE` sentinel and applies the
//!    recursion-guard logic above. If the sentinel says "skip this
//!    call," returns `Ok(())` immediately.
//! 3. Runs `cargo metadata --format-version=1` and locates `bin_crate`.
//!    Enumerates its `[[bin]]` targets.
//! 4. Runs `$CARGO build --bins -p <bin_crate>` (plus `--release` when
//!    the outer `PROFILE == "release"`) with
//!    `CARGO_TARGET_DIR=$OUT_DIR/rudzio-bin-cache` and the sentinel set
//!    on the child environment — a dedicated target dir so there's no
//!    lock contention with the outer cargo invocation, and the sentinel
//!    so step 2 above can trip on any re-entrant call. Cargo's own
//!    incremental cache lives inside that dir, so repeat build-script
//!    runs are cheap when nothing changed.
//! 5. Verifies each bin landed at the expected path
//!    (`<target>/<profile>/<name>`) and emits
//!    `cargo:rustc-env=CARGO_BIN_EXE_<name>=<abs path>`.
//! 6. Emits `cargo:rerun-if-changed=<bin crate's Cargo.toml and src/>`
//!    so touching the bin crate re-triggers this build script.
//!
//! Every failure (missing env var, package not in metadata, no bin
//! targets, nested cargo exit != 0, missing expected output) surfaces
//! as an explicit [`Error`] with context. There are no silent fallbacks:
//! if the helper can't do its job, your build breaks loudly.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::{MetadataCommand, TargetKind};

/// Sentinel env var set on every nested `cargo build` spawned by
/// [`expose_bins`]. Its presence in the ambient env at the top of
/// [`expose_bins`] means some enclosing invocation is already driving
/// the build; the current call must not spawn a fresh nested cargo or
/// the whole thing will recurse unboundedly.
///
/// The `__` prefix signals "internal to rudzio, don't touch" — users
/// should never set or read this variable themselves.
const NESTED_SENTINEL_ENV: &str = "__RUDZIO_EXPOSE_BINS_ACTIVE";

/// Error returned by [`expose_bins`]. Wraps the underlying failure
/// cause (missing env var, cargo metadata failure, nested build failure,
/// etc.) with a human-readable message.
#[derive(Debug)]
pub struct Error {
    message: String,
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        // `Box<dyn Error + Send + Sync>::deref` yields `&(dyn Error + Send + Sync)`,
        // which coerces directly to `&(dyn Error + 'static)`.
        self.source.as_deref().map(|err| {
            let err: &(dyn StdError + 'static) = err;
            err
        })
    }
}

/// Convenience alias used across this crate's surface.
pub type Result<T> = std::result::Result<T, Error>;

/// Build `bin_crate`'s `[[bin]]` targets into a sandboxed cache inside
/// the caller's `OUT_DIR` and emit `cargo:rustc-env=CARGO_BIN_EXE_<n>`
/// directives so `env!("CARGO_BIN_EXE_<n>")` resolves in the caller's
/// compiled sources.
///
/// `bin_crate` may be the calling crate itself — useful for the
/// `cargo test --lib` + `#[cfg(test)] #[rudzio::main]` pattern where
/// cargo doesn't auto-populate `CARGO_BIN_EXE_*`. The recursion that
/// would otherwise trigger is broken by the sentinel env var documented
/// in the crate-level docs.
///
/// # Errors
///
/// Returns an error if any of the following go wrong (with context):
/// - A required cargo-populated env var (`CARGO_MANIFEST_DIR`, `OUT_DIR`,
///   `PROFILE`, `CARGO`, `CARGO_PKG_NAME`) is missing.
/// - `cargo metadata` fails to run or produce parseable output.
/// - `bin_crate` is not listed in metadata.
/// - `bin_crate` declares no `[[bin]]` targets.
/// - The nested `cargo build` invocation exits non-zero.
/// - A bin's expected output file does not exist after the nested build.
pub fn expose_bins(bin_crate: &str) -> Result<()> {
    let env = BuildEnv::capture()?;
    let sentinel = std::env::var_os(NESTED_SENTINEL_ENV);
    match decide_sentinel_action(sentinel.as_deref(), bin_crate, &env.pkg_name) {
        SentinelAction::Proceed => {}
        SentinelAction::SilentOk => return Ok(()),
        SentinelAction::WarnAndOk => {
            // `cargo:warning=` is cargo's only in-tree surface for
            // non-fatal build-script messages. Emit one so a genuine
            // cycle isn't hidden behind a silent Ok — the user needs
            // the signal to investigate.
            println!(
                "cargo:warning=rudzio::build::expose_bins: detected a re-entrant \
                 call for `{bin_crate}` while building `{}` (the \
                 `{NESTED_SENTINEL_ENV}` sentinel is set, meaning an enclosing \
                 `expose_bins` invocation spawned this build). Returning Ok \
                 without a nested `cargo build` to prevent runaway recursion. \
                 If you did not expect this, look for a build-script cycle \
                 where two crates' `expose_bins` calls trigger one another.",
                env.pkg_name
            );
            return Ok(());
        }
    }
    // NOT `.no_deps()`: when the aggregator is its own workspace, the
    // bin crate shows up only as a dependency in this view, and
    // `no_deps()` would filter it out.
    let metadata = MetadataCommand::new()
        .current_dir(&env.manifest_dir)
        .exec()
        .map_err(|e| Error::with_source("`cargo metadata` failed", e))?;

    let pkg = metadata
        .packages
        .iter()
        .find(|p| p.name.as_str() == bin_crate)
        .ok_or_else(|| {
            Error::new(format!(
                "`cargo metadata` did not list a package named `{bin_crate}`. \
                 Add it to the aggregator's `[dependencies]` or \
                 `[dev-dependencies]` so it shows up in the metadata view."
            ))
        })?;

    let bin_targets: Vec<&cargo_metadata::Target> = pkg
        .targets
        .iter()
        .filter(|t| t.kind.iter().any(|k| matches!(k, TargetKind::Bin)))
        .collect();
    if bin_targets.is_empty() {
        return Err(Error::new(format!(
            "package `{bin_crate}` declares no `[[bin]]` targets"
        )));
    }

    let nested_target: PathBuf = env.out_dir.join("rudzio-bin-cache");
    let profile_flag: ProfileFlag = ProfileFlag::from_env_profile(&env.profile)?;

    let mut cmd = Command::new(&env.cargo);
    let _: &mut Command = cmd
        .arg("build")
        .arg("--bins")
        .arg("-p")
        .arg(bin_crate)
        .env("CARGO_TARGET_DIR", &nested_target)
        // Sentinel is what the early-return check at the top of
        // `expose_bins` trips on when cargo re-enters this build script
        // (or any other crate's build script that also calls
        // `expose_bins`) as part of the nested build below. Without it
        // a misconfigured consumer or an exotic cross-crate cycle can
        // accumulate `rudzio-bin-cache/.../rudzio-bin-cache/...`
        // indefinitely — see the module-level docs.
        .env(NESTED_SENTINEL_ENV, "1");
    if let Some(flag) = profile_flag.cli_flag() {
        let _: &mut Command = cmd.arg(flag);
    }

    let status = cmd.status().map_err(|e| {
        Error::with_source(
            format!(
                "failed to spawn nested `cargo build --bins -p {bin_crate}` \
                 (CARGO={:?})",
                env.cargo
            ),
            e,
        )
    })?;
    if !status.success() {
        return Err(Error::new(format!(
            "nested `cargo build --bins -p {bin_crate}` exited with {status}"
        )));
    }

    let bin_dir = nested_target.join(profile_flag.output_subdir());
    for target in &bin_targets {
        let bin_path = bin_dir.join(&target.name);
        if !bin_path.exists() {
            return Err(Error::new(format!(
                "nested build reported success but bin `{}` is missing at `{}`",
                target.name,
                bin_path.display()
            )));
        }
        println!(
            "cargo:rustc-env=CARGO_BIN_EXE_{}={}",
            target.name,
            bin_path.display()
        );
    }

    println!(
        "cargo:rerun-if-changed={}",
        pkg.manifest_path.as_std_path().display()
    );
    if let Some(pkg_root) = pkg.manifest_path.as_std_path().parent() {
        let src_dir: PathBuf = pkg_root.join("src");
        if src_dir.exists() {
            println!("cargo:rerun-if-changed={}", src_dir.display());
        }
    }

    Ok(())
}

/// Same as [`expose_bins`] but reads `CARGO_PKG_NAME` so the caller
/// doesn't have to hardcode their own crate's name. Intended for the
/// `cargo test --lib` + `#[cfg(test)] #[rudzio::main]` pattern where
/// cargo doesn't auto-populate `CARGO_BIN_EXE_*` for the `--lib` test
/// binary.
///
/// ```rust,no_run
/// // build.rs — make this crate's own bins reachable to tests run by
/// // `cargo test --lib`.
/// fn main() -> Result<(), rudzio::build::Error> {
///     rudzio::build::expose_self_bins()
/// }
/// ```
///
/// # Errors
///
/// Same failure modes as [`expose_bins`]; additionally errors if
/// `CARGO_PKG_NAME` is missing from the env (build scripts without a
/// cargo parent).
pub fn expose_self_bins() -> Result<()> {
    let pkg_name = require_string_env("CARGO_PKG_NAME")?;
    expose_bins(&pkg_name)
}

/// Required build-script environment variables, read up-front so errors
/// surface in one place rather than at the site of first use.
struct BuildEnv {
    manifest_dir: PathBuf,
    out_dir: PathBuf,
    profile: String,
    cargo: OsString,
    /// `CARGO_PKG_NAME` — the crate whose build script is currently
    /// running. Compared against `bin_crate` when the sentinel is set
    /// to distinguish expected same-crate re-entry (silent Ok) from
    /// unexpected cross-crate re-entry (warn + Ok).
    pkg_name: String,
}

impl BuildEnv {
    fn capture() -> Result<Self> {
        Ok(Self {
            manifest_dir: require_path_env("CARGO_MANIFEST_DIR")?,
            out_dir: require_path_env("OUT_DIR")?,
            profile: require_string_env("PROFILE")?,
            cargo: std::env::var_os("CARGO").ok_or_else(|| {
                Error::new(
                    "`CARGO` env var missing; `expose_bins` must be called \
                     from a cargo-driven build script",
                )
            })?,
            pkg_name: require_string_env("CARGO_PKG_NAME")?,
        })
    }
}

/// What [`expose_bins`] should do based on the sentinel env var and the
/// relationship between `bin_crate` and the calling crate.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelAction {
    /// No nested invocation in progress — run the full
    /// metadata/build/emit pipeline.
    Proceed,
    /// Nested invocation is for the same crate the caller is already
    /// building. Return `Ok(())` silently — the outer call will emit
    /// the env vars after its nested cargo finishes. This is the
    /// expected re-entry triggered by `expose_bins("self-crate")` from
    /// the same crate's own build.rs.
    SilentOk,
    /// Sentinel is set but `bin_crate` differs from the calling crate
    /// — possible cross-crate cycle. Emit a `cargo:warning=` and
    /// return `Ok(())` to break the recursion.
    WarnAndOk,
}

/// Decide what [`expose_bins`] should do given the current sentinel
/// state and crate identity.
///
/// Extracted as a pure function for unit testability — `expose_bins`
/// reads the sentinel and the calling crate's `CARGO_PKG_NAME` once,
/// then feeds them through here.
#[doc(hidden)]
pub fn decide_sentinel_action(
    sentinel: Option<&std::ffi::OsStr>,
    bin_crate: &str,
    current_pkg: &str,
) -> SentinelAction {
    if !sentinel_indicates_nested_call(sentinel) {
        return SentinelAction::Proceed;
    }
    if bin_crate == current_pkg {
        SentinelAction::SilentOk
    } else {
        SentinelAction::WarnAndOk
    }
}

/// Returns `true` when the sentinel env var looks like it was set by
/// an enclosing `expose_bins` invocation (present and non-empty).
#[doc(hidden)]
pub fn sentinel_indicates_nested_call(value: Option<&std::ffi::OsStr>) -> bool {
    matches!(value, Some(v) if !v.is_empty())
}

fn require_string_env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|e| {
        Error::with_source(
            format!(
                "`{name}` env var missing; `expose_bins` must be called from \
                 a cargo-driven build script"
            ),
            e,
        )
    })
}

fn require_path_env(name: &str) -> Result<PathBuf> {
    require_string_env(name).map(PathBuf::from)
}

/// Maps cargo's `PROFILE` env var (set to `debug` or `release` for
/// build scripts) onto the flag we pass to the nested `cargo build` and
/// onto the output subdirectory cargo writes the binary into.
enum ProfileFlag {
    Debug,
    Release,
}

impl ProfileFlag {
    fn from_env_profile(profile: &str) -> Result<Self> {
        match profile {
            "debug" => Ok(Self::Debug),
            "release" => Ok(Self::Release),
            other => Err(Error::new(format!(
                "unrecognised `PROFILE={other}`; rudzio-build only knows how \
                 to forward `debug` or `release`. Custom profiles with \
                 non-standard `inherits` would need explicit handling."
            ))),
        }
    }

    fn cli_flag(&self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("--release"),
        }
    }

    fn output_subdir(&self) -> &'static Path {
        match self {
            Self::Debug => Path::new("debug"),
            Self::Release => Path::new("release"),
        }
    }
}

