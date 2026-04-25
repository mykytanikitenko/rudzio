//! Build-script helper: make *another* crate's `[[bin]]` targets
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
//! # When you do NOT need this
//!
//! If the `[[bin]]` targets live in the *same* crate as the integration
//! tests that spawn them, cargo already sets `CARGO_BIN_EXE_<name>`
//! automatically — nothing to wire up. This holds for the single-crate
//! aggregation pattern too (`tests/main.rs` with `harness = false`
//! pulling sources via `#[path]`): the test target belongs to the bin's
//! own crate, so the env var is populated before rustc sees the source.
//!
//! Calling `expose_bins(<current crate>)` from that same crate's
//! `build.rs` would be worse than redundant — it would recurse
//! indefinitely. `expose_bins` shells out to
//! `cargo build --bins -p <bin_crate>`; if `<bin_crate>` is the crate
//! whose build script just ran, that nested cargo re-runs this same
//! build script, which calls `expose_bins` again, ad infinitum. An
//! in-the-wild report caught this only after 8.4 GB of nested
//! `rudzio-bin-cache/.../rudzio-bin-cache/...` target directories had
//! accumulated on disk, because cargo's default output is quiet about
//! repeated rebuilds.
//!
//! Two layers of defense stop the recursion:
//!
//! 1. **Direct self-use (`bin_crate == CARGO_PKG_NAME`)** — rejected
//!    loudly with an [`Error`] at the outer invocation site. The error
//!    message names the offending crate and points the user at the
//!    actual fix (drop the `expose_bins` call; cargo already sets
//!    `CARGO_BIN_EXE_<name>` for same-crate integration tests).
//! 2. **Re-entry sentinel** — before spawning the nested cargo,
//!    `expose_bins` sets `__RUDZIO_EXPOSE_BINS_ACTIVE=1` on the child
//!    process environment. Cargo propagates that env var through to any
//!    further build scripts it runs (including a re-run of the caller's
//!    own build script, if the sandbox happens to re-trigger it). If
//!    `expose_bins` is entered while the sentinel is already set, it
//!    emits a `cargo::warning=...` and returns `Ok(())` immediately
//!    instead of spawning another nested cargo. This catches exotic
//!    cross-crate cycles (crate A's `build.rs` calls `expose_bins("B")`,
//!    B's `build.rs` calls `expose_bins("C")`, C's `build.rs` calls
//!    `expose_bins("A")`, …) that the `CARGO_PKG_NAME` check would miss.
//!    The sentinel check returns `Ok(())` rather than an error because,
//!    at that point, stopping the recursion takes priority over
//!    reporting the misconfiguration; the warning gives the user the
//!    signal they need to investigate.
//!
//! # Requirements for the calling crate
//!
//! - `[build-dependencies]`: `rudzio = { version = "...", default-features = false, features = ["build"] }`.
//! - `[dependencies]` (or `[dev-dependencies]`): the bin crate, so
//!   `cargo metadata` lists it in the workspace view from the
//!   aggregator's manifest.
//! - The bin crate must be a *different* package from the one calling
//!   `expose_bins` (see "When you do NOT need this" above).
//!
//! # What it does, step by step
//!
//! 1. Short-circuits to `Ok(())` + a `cargo::warning` line if the
//!    re-entry sentinel (`__RUDZIO_EXPOSE_BINS_ACTIVE`) is set, which
//!    means an enclosing `expose_bins` invocation is already driving
//!    the build. Stops unbounded recursion dead.
//! 2. Reads `CARGO_MANIFEST_DIR`, `OUT_DIR`, `PROFILE`, `CARGO`, and
//!    `CARGO_PKG_NAME` from the build-script env (all populated by
//!    cargo).
//! 3. Refuses to proceed if `bin_crate == CARGO_PKG_NAME` — see above.
//! 4. Runs `cargo metadata --format-version=1` and locates
//!    `bin_crate`. Enumerates its `[[bin]]` targets.
//! 5. Runs `$CARGO build --bins -p <bin_crate>` (plus `--release` when
//!    the outer `PROFILE == "release"`) with
//!    `CARGO_TARGET_DIR=$OUT_DIR/rudzio-bin-cache` and the re-entry
//!    sentinel set on the child environment — a dedicated target dir
//!    so there's no lock contention with the outer cargo invocation,
//!    and the sentinel so step 1 above can trip on any re-entrant call.
//!    Cargo's own incremental cache lives inside that dir, so repeat
//!    build-script runs are cheap when nothing changed.
//! 6. Verifies each bin landed at the expected path
//!    (`<target>/<profile>/<name>`) and emits
//!    `cargo:rustc-env=CARGO_BIN_EXE_<name>=<abs path>`.
//! 7. Emits `cargo:rerun-if-changed=<bin crate's Cargo.toml and src/>`
//!    so touching the bin crate re-triggers this build script.
//!
//! Every failure (missing env var, same-crate self-use, package not in
//! metadata, no bin targets, nested cargo exit != 0, missing expected
//! output) surfaces as an explicit [`Error`] with context. There are no
//! silent fallbacks: if the helper can't do its job, your build breaks
//! loudly.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::{MetadataCommand, TargetKind};

/// Sentinel env var set on every nested `cargo build` spawned by
/// [`expose_bins`]. Its presence in the ambient env at the top of
/// [`expose_bins`] means some enclosing invocation is already driving
/// the build, and the current call must not spawn a fresh nested
/// cargo or the whole thing will recurse unboundedly.
///
/// The `__` prefix signals "internal to rudzio, don't touch" — users
/// should never set or read this variable themselves. If they want to
/// detect the nested context from their own code, they can also look
/// at `CARGO_TARGET_DIR` for the `rudzio-bin-cache` substring (the
/// pre-fix workaround), but with the sentinel in place no such
/// workaround should be needed.
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
/// Intended to be called from a build script in a crate that is *not*
/// `bin_crate`. The call is rejected when `bin_crate` matches the
/// current crate's `CARGO_PKG_NAME` — see the crate-level docs and the
/// "Errors" section below for why.
///
/// # Errors
///
/// Returns an error if any of the following go wrong (with context):
/// - A required cargo-populated env var (`CARGO_MANIFEST_DIR`, `OUT_DIR`,
///   `PROFILE`, `CARGO`, `CARGO_PKG_NAME`) is missing.
/// - `bin_crate == CARGO_PKG_NAME`. Recursing into a
///   `cargo build --bins -p <current crate>` from inside that crate's
///   own build script would re-trigger this function indefinitely, so
///   the case is rejected up front with a message telling the caller
///   to drop the `expose_bins` call (cargo already sets
///   `CARGO_BIN_EXE_<name>` for the crate's own integration tests).
/// - `cargo metadata` fails to run or produce parseable output.
/// - `bin_crate` is not listed in metadata.
/// - `bin_crate` declares no `[[bin]]` targets.
/// - The nested `cargo build` invocation exits non-zero.
/// - A bin's expected output file does not exist after the nested build.
pub fn expose_bins(bin_crate: &str) -> Result<()> {
    let sentinel = std::env::var_os(NESTED_SENTINEL_ENV);
    if sentinel_indicates_nested_call(sentinel.as_deref()) {
        // A `cargo:warning=` line is cargo's only in-tree surface for
        // surfacing non-fatal build-script messages to the user. Emit
        // one so the situation isn't invisible — silent Ok would hide
        // a genuine misconfiguration that the user should see.
        println!(
            "cargo:warning=rudzio::build::expose_bins: detected a re-entrant \
             call for `{bin_crate}` (the `{NESTED_SENTINEL_ENV}` sentinel is \
             set, meaning an enclosing `expose_bins` invocation spawned this \
             build). Returning Ok without a nested `cargo build` to prevent \
             runaway recursion. If you did not expect this, look for a \
             build-script cycle where two crates' `expose_bins` calls \
             trigger one another."
        );
        return Ok(());
    }
    let env = BuildEnv::capture()?;
    guard_against_self_use(bin_crate, &env.pkg_name)?;
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

/// Required build-script environment variables, read up-front so errors
/// surface in one place rather than at the site of first use.
struct BuildEnv {
    manifest_dir: PathBuf,
    out_dir: PathBuf,
    profile: String,
    cargo: OsString,
    /// `CARGO_PKG_NAME` — the crate whose build script is currently
    /// running. Compared against `bin_crate` to refuse same-crate
    /// self-use before it spirals into unbounded build-script recursion.
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

/// Returns `true` when the sentinel env var looks like it was set by
/// an enclosing `expose_bins` invocation (present and non-empty).
///
/// Extracted for unit testability — `expose_bins` itself reads the env
/// once and feeds the resulting `Option<&OsStr>` through this helper,
/// so the test suite can drive it with synthetic inputs without
/// touching process-global env state.
fn sentinel_indicates_nested_call(value: Option<&std::ffi::OsStr>) -> bool {
    matches!(value, Some(v) if !v.is_empty())
}

/// Reject `expose_bins` called for the crate that owns the build
/// script that called it.
///
/// Letting the call through would spawn
/// `cargo build --bins -p <current crate>`, which re-invokes this same
/// build script, which calls `expose_bins` again, which spawns another
/// nested cargo — each layer forking another layer until the OS tears
/// the process tree down. The nested target dir does not help, because
/// it doesn't change the fact that cargo must execute the build script
/// as part of building the bin target.
///
/// Same-crate use is also redundant: cargo already sets
/// `CARGO_BIN_EXE_<name>` for integration tests of the crate that
/// declares the `[[bin]]`, so the caller has nothing to gain. We
/// surface that guidance in the error message rather than silently
/// no-op'ing, since a silent no-op could mask a genuinely miswired
/// aggregator layout.
fn guard_against_self_use(bin_crate: &str, current_pkg: &str) -> Result<()> {
    if bin_crate != current_pkg {
        return Ok(());
    }
    Err(Error::new(format!(
        "refusing to call `expose_bins(\"{bin_crate}\")` from the build \
         script of `{bin_crate}` itself. Cargo already sets \
         `CARGO_BIN_EXE_<name>` for integration tests of the crate that \
         declares the `[[bin]]`, so the call is redundant. Letting it \
         through would also recurse indefinitely: the nested \
         `cargo build --bins -p {bin_crate}` re-runs this same build \
         script, which calls `expose_bins` again. `expose_bins` is only \
         needed when the aggregator crate is *different* from the crate \
         that owns the bin targets — drop the call, or point it at the \
         actual bin-owning crate."
    )))
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{Result, guard_against_self_use, sentinel_indicates_nested_call};

    /// Regression guard for the "call `expose_bins` on your own crate"
    /// footgun. Without the check the function shells out to
    /// `cargo build --bins -p <current crate>`, which re-runs the same
    /// build script, which calls `expose_bins` again — each nested
    /// cargo forks another one until the OS kills the tree.
    #[test]
    fn refuses_same_crate_to_prevent_build_script_recursion() {
        let err: super::Error = guard_against_self_use("file-v3", "file-v3")
            .expect_err("same-crate use must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("file-v3"),
            "error should name the offending crate so the fix site is obvious; got: {msg}"
        );
        assert!(
            msg.contains("recursion") || msg.contains("build script"),
            "error should explain why self-use is rejected; got: {msg}"
        );
    }

    /// Normal use: an aggregator crate exposing a different crate's
    /// bins. Must not trip the recursion guard.
    #[test]
    fn accepts_different_bin_crate() {
        let ok: Result<()> =
            guard_against_self_use("rudzio-fixtures", "rudzio-test-runner");
        assert!(ok.is_ok(), "cross-crate use must pass the guard: {ok:?}");
    }

    /// The sentinel env var being present (with any non-empty value) is
    /// how the outer `expose_bins` tells its descendants "I'm already
    /// driving the nested build — don't start another one". The
    /// detector should recognise a plain `"1"` and any other non-empty
    /// value the same way, so consumers can't accidentally bypass it by
    /// writing something non-canonical.
    #[test]
    fn sentinel_detector_recognises_any_non_empty_value() {
        assert!(sentinel_indicates_nested_call(Some(OsStr::new("1"))));
        assert!(sentinel_indicates_nested_call(Some(OsStr::new("yes"))));
        assert!(sentinel_indicates_nested_call(Some(OsStr::new("0"))));
    }

    /// When the sentinel is absent (or cargo happened to pass an empty
    /// string — `std::env::var_os` returns `Some("")` in that edge
    /// case) the detector must NOT treat the call as nested, so normal
    /// first-level invocations aren't accidentally short-circuited.
    #[test]
    fn sentinel_detector_ignores_absent_or_empty() {
        assert!(!sentinel_indicates_nested_call(None));
        assert!(!sentinel_indicates_nested_call(Some(OsStr::new(""))));
    }
}
