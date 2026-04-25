//! Build-script helper: make another crate's `[[bin]]` targets
//! reachable from `env!("CARGO_BIN_EXE_<name>")` in the caller's
//! integration tests.
//!
//! Enabled by the `build` feature — declare rudzio as a build-dep with
//! that feature on. Cargo only sets `CARGO_BIN_EXE_<name>` for
//! integration tests of the crate that declares the `[[bin]]`. If
//! you're running a workspace-wide test-runner binary that needs to
//! invoke bins from a *different* crate, either (a) re-declare them as
//! `[[bin]]`s in your aggregator (tedious, and duplicated when the
//! source crate adds/removes bins) or (b) call [`expose_bins`] from a
//! `build.rs`.
//!
//! ```rust,no_run
//! // aggregator/build.rs
//! fn main() -> Result<(), rudzio::build::Error> {
//!     rudzio::build::expose_bins("my-e2e-crate")
//! }
//! ```
//!
//! Requirements for the calling crate:
//! - `[build-dependencies]`: `rudzio = { version = "...", default-features = false, features = ["build"] }`.
//! - `[dependencies]` (or `[dev-dependencies]`): the bin crate, so
//!   `cargo metadata` lists it in the workspace view from the
//!   aggregator's manifest.
//!
//! What it does, step by step:
//! 1. Reads `CARGO_MANIFEST_DIR`, `OUT_DIR`, `PROFILE`, and `CARGO` from
//!    the build-script env (all populated by cargo).
//! 2. Runs `cargo metadata --format-version=1 --no-deps` and locates
//!    `bin_crate`. Enumerates its `[[bin]]` targets.
//! 3. Runs `$CARGO build --bins -p <bin_crate>` (plus `--release` when
//!    the outer `PROFILE == "release"`) with
//!    `CARGO_TARGET_DIR=$OUT_DIR/rudzio-bin-cache` — a dedicated target
//!    dir so there's no lock contention with the outer cargo invocation.
//!    Cargo's own incremental cache lives inside that dir, so repeat
//!    build-script runs are cheap when nothing changed.
//! 4. Verifies each bin landed at the expected path
//!    (`<target>/<profile>/<name>`) and emits
//!    `cargo:rustc-env=CARGO_BIN_EXE_<name>=<abs path>`.
//! 5. Emits `cargo:rerun-if-changed=<bin crate's Cargo.toml and src/>`
//!    so touching the bin crate re-triggers this build script.
//!
//! Every failure (missing env var, package not in metadata, no bin
//! targets, nested cargo exit != 0, missing expected output) surfaces
//! as an explicit [`Error`] with context. There are no silent
//! fallbacks: if the helper can't do its job, your build breaks
//! loudly.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::{MetadataCommand, TargetKind};

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
/// Intended to be called from a build script; see the crate-level docs
/// for the full requirements and behaviour.
///
/// # Errors
///
/// Returns an error if any of the following go wrong (with context):
/// - A required cargo-populated env var (`CARGO_MANIFEST_DIR`, `OUT_DIR`,
///   `PROFILE`, `CARGO`) is missing.
/// - `cargo metadata` fails to run or produce parseable output.
/// - `bin_crate` is not listed in metadata.
/// - `bin_crate` declares no `[[bin]]` targets.
/// - The nested `cargo build` invocation exits non-zero.
/// - A bin's expected output file does not exist after the nested build.
pub fn expose_bins(bin_crate: &str) -> Result<()> {
    let env = BuildEnv::capture()?;
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
        .env("CARGO_TARGET_DIR", &nested_target);
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

/// Required build-script environment variables, read up-front so errors
/// surface in one place rather than at the site of first use.
struct BuildEnv {
    manifest_dir: PathBuf,
    out_dir: PathBuf,
    profile: String,
    cargo: OsString,
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
        })
    }
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
