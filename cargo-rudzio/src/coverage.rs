//! Coverage-environment handling for `cargo rudzio test`.
//!
//! `cargo llvm-cov` has two ways to instrument a build. The `show-env`
//! contract (`eval "$(cargo llvm-cov show-env)"`, then run the build tool,
//! then `cargo llvm-cov report`) hands the build to an external tool — here,
//! cargo-rudzio. Under that contract `report` collects its inputs from fixed
//! locations:
//!
//! * `.profraw` files — a NON-recursive glob at
//!   `$CARGO_LLVM_COV_TARGET_DIR/*.profraw`; and
//! * object files (linked executables) — a recursive walk of
//!   `$CARGO_LLVM_COV_TARGET_DIR/<profile>` (default `debug`).
//!
//! The `.profraw` files are dumped by the running instrumented binary to the
//! absolute path baked into `LLVM_PROFILE_FILE` at `show-env` time, so the
//! only placement cargo-rudzio controls is the object files. Without this
//! module, `run_test` anchors the inner aggregator cargo build at the
//! per-aggregator `rudzio-auto-runner-<hash>/target` subdir (which keeps
//! concurrent non-coverage invocations isolated), so the instrumented objects
//! land there and `report` finds nothing.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Marker emitted by `cargo llvm-cov show-env` to signal that the build runs
/// under LLVM source-based coverage instrumentation.
pub const COVERAGE_ENV: &str = "CARGO_LLVM_COV";
/// `show-env`-emitted directory `cargo llvm-cov report` searches for objects.
pub const COVERAGE_TARGET_DIR_ENV: &str = "CARGO_LLVM_COV_TARGET_DIR";
/// `show-env`-emitted build-script directory. Set only when `build.build-dir`
/// is configured; otherwise absent.
pub const COVERAGE_BUILD_DIR_ENV: &str = "CARGO_LLVM_COV_BUILD_DIR";

/// Whether coverage instrumentation is active in the current process.
///
/// `cargo llvm-cov show-env` sets [`COVERAGE_ENV`] so a child process can
/// recognise that it must build into [`COVERAGE_TARGET_DIR_ENV`] rather than
/// its private per-aggregator subdir.
#[must_use]
pub fn is_active() -> bool {
    env::var_os(COVERAGE_ENV).is_some()
}

/// Resolved target/build directories for the inner aggregator cargo build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregatorDirs {
    /// Directory cargo places the linked aggregator binary and objects in.
    pub target_dir: PathBuf,
    /// `None` means "do not override `CARGO_BUILD_BUILD_DIR`".
    pub build_dir: Option<PathBuf>,
}

/// Resolve the inner aggregator cargo build's target/build directories.
///
/// `base_output_dir` is `Plan::default_output_dir()` — the per-aggregator
/// `rudzio-auto-runner-<hash>` directory. Without coverage the inner build
/// anchors `target/` and `build-dir/` under it (parallel-invocation
/// isolation). With coverage the inner build anchors at
/// [`COVERAGE_TARGET_DIR_ENV`] so `cargo llvm-cov report` finds the
/// instrumented objects and `.profraw` files.
///
/// `coverage_target_dir` and `coverage_build_dir` are the raw values read
/// from the process environment (or `None` when unset). They are parameters
/// rather than read here so the pure logic is unit-testable in isolation.
///
/// # Errors
///
/// Fails loudly when coverage is active but `CARGO_LLVM_COV_TARGET_DIR` is
/// unset: that is a partial coverage environment, and falling back to the
/// per-aggregator subdir would silently yield an empty coverage report.
pub fn resolve_aggregator_dirs(
    coverage_active: bool,
    coverage_target_dir: Option<&OsStr>,
    coverage_build_dir: Option<&OsStr>,
    base_output_dir: &Path,
) -> Result<AggregatorDirs> {
    if !coverage_active {
        return Ok(AggregatorDirs {
            target_dir: base_output_dir.join("target"),
            build_dir: Some(base_output_dir.join("build-dir")),
        });
    }
    let target_dir = coverage_target_dir.ok_or_else(|| {
        anyhow::anyhow!(
            "`{COVERAGE_ENV}` is set but `{COVERAGE_TARGET_DIR_ENV}` is not: the coverage \
             environment is incomplete. Re-run `cargo llvm-cov show-env` and eval its output \
             before `cargo rudzio test`, or unset `{COVERAGE_ENV}` to run without coverage."
        )
    })?;
    Ok(AggregatorDirs {
        target_dir: PathBuf::from(target_dir),
        build_dir: coverage_build_dir.map(PathBuf::from),
    })
}

#[cfg(test)]
mod tests {
    use super::{COVERAGE_TARGET_DIR_ENV, resolve_aggregator_dirs};
    use std::ffi::OsStr;
    use std::path::Path;

    const BASE: &str = "/outer/target/rudzio-auto-runner-0123456789abcdef-0123456789abcdef";

    #[test]
    fn non_coverage_anchors_under_base_output_dir() {
        let dirs =
            resolve_aggregator_dirs(false, None, None, Path::new(BASE)).expect("resolve dirs");
        assert_eq!(
            dirs.target_dir,
            Path::new("/outer/target/rudzio-auto-runner-0123456789abcdef-0123456789abcdef/target")
        );
        assert_eq!(
            dirs.build_dir.as_deref(),
            Some(Path::new(
                "/outer/target/rudzio-auto-runner-0123456789abcdef-0123456789abcdef/build-dir"
            ))
        );
    }

    #[test]
    fn coverage_uses_llvm_cov_target_dir_even_when_it_conflicts_with_base() {
        // The coverage target dir is the directory `cargo llvm-cov report`
        // searches; it has no relationship to `base_output_dir`. This is the
        // "conflicting CARGO_TARGET_DIR" case: the outer target dir (BASE's
        // ancestor) differs from the coverage target dir.
        let dirs = resolve_aggregator_dirs(
            true,
            Some(OsStr::new("/var/mnt/data-cache/cargo-target")),
            Some(OsStr::new("/var/mnt/data-cache/cargo/build-dir")),
            Path::new(BASE),
        )
        .expect("resolve dirs");
        assert_eq!(dirs.target_dir, Path::new("/var/mnt/data-cache/cargo-target"));
        assert_eq!(
            dirs.build_dir.as_deref(),
            Some(Path::new("/var/mnt/data-cache/cargo/build-dir"))
        );
    }

    #[test]
    fn coverage_without_build_dir_does_not_override_build_dir_env() {
        let dirs = resolve_aggregator_dirs(
            true,
            Some(OsStr::new("/cov-target")),
            None,
            Path::new(BASE),
        )
        .expect("resolve dirs");
        assert_eq!(dirs.target_dir, Path::new("/cov-target"));
        assert_eq!(dirs.build_dir, None);
    }

    #[test]
    fn coverage_without_target_dir_fails_loudly() {
        let err = resolve_aggregator_dirs(true, None, None, Path::new(BASE)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(COVERAGE_TARGET_DIR_ENV), "error should name the missing env var: {msg}");
        assert!(msg.contains("incomplete"), "error should say the env is incomplete: {msg}");
    }
}
