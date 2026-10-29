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

#[cfg(any(test, rudzio_test))]
use rudzio::common::context::{Suite, Test};
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::futures::ThreadPool;
#[cfg(all(any(test, rudzio_test), any(target_os = "linux", target_os = "macos"),))]
use rudzio::runtime::monoio;
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::{async_std, compio, embassy, smol};

/// Marker emitted by `cargo llvm-cov show-env` to signal that the build runs
/// under LLVM source-based coverage instrumentation.
pub const COVERAGE_ENV: &str = "CARGO_LLVM_COV";
/// `show-env`-emitted directory `cargo llvm-cov report` searches for objects.
pub const COVERAGE_TARGET_DIR_ENV: &str = "CARGO_LLVM_COV_TARGET_DIR";
/// `show-env`-emitted build-script directory. Set only when `build.build-dir`
/// is configured; otherwise absent.
pub const COVERAGE_BUILD_DIR_ENV: &str = "CARGO_LLVM_COV_BUILD_DIR";
/// `show-env`-emitted comma-separated crate-name list the rustc wrapper
/// matches against `--crate-name` to decide what to instrument.
pub const WRAPPER_CRATE_NAMES_ENV: &str = "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES";
/// Cargo environment variable carrying `--remap-path-prefix` rustflags for
/// the inner aggregator build.
pub const RUSTFLAGS_ENV: &str = "RUSTFLAGS";
/// `show-env`-emitted rustc wrapper that instruments the build.
pub const RUSTC_WRAPPER_ENV: &str = "RUSTC_WRAPPER";
/// `show-env`-emitted profile-output pattern the instrumented runner dumps
/// its `.profraw` to.
pub const LLVM_PROFILE_FILE_ENV: &str = "LLVM_PROFILE_FILE";

/// Resolved target/build directories for the inner aggregator cargo build.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AggregatorDirs {
    /// `None` means "do not override `CARGO_BUILD_BUILD_DIR`".
    pub build_dir: Option<PathBuf>,
    /// Directory cargo places the linked aggregator binary and objects in.
    pub target_dir: PathBuf,
}

/// Append generated crate names to `__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES`.
///
/// `generated` holds the crate names the aggregator build produces but
/// `cargo llvm-cov show-env` cannot know (the runner bin and per-bin bridge
/// libs). The cargo-llvm-cov rustc wrapper instruments a crate only when its
/// `--crate-name` appears in the list, so without the runner name the final
/// binary links no profiler runtime and emits no `.profraw`. Existing entries
/// are preserved verbatim and already-present names are not duplicated, so
/// the augmentation is idempotent across re-runs.
#[must_use]
#[inline]
pub fn augment_crate_names(existing: &str, generated: &[String]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for name in existing
        .split(',')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    for name in generated {
        if !names.contains(&name.as_str()) {
            names.push(name.as_str());
        }
    }
    names.join(",")
}

/// Whether coverage instrumentation is active in the current process.
///
/// `cargo llvm-cov show-env` sets [`COVERAGE_ENV`] so a child process can
/// recognise that it must build into [`COVERAGE_TARGET_DIR_ENV`] rather than
/// its private per-aggregator subdir.
#[must_use]
#[inline]
pub fn is_active() -> bool {
    env::var_os(COVERAGE_ENV).is_some()
}

/// Verify the `show-env` contract's wrapper and profile-file variables are
/// present in the current process when coverage is active.
///
/// `eval "$(cargo llvm-cov show-env)"` without `--sh` emits bare
/// `NAME='value'` assignments that `eval` makes shell-local (not exported),
/// so a child process can see [`COVERAGE_ENV`] while [`RUSTC_WRAPPER_ENV`]
/// and [`LLVM_PROFILE_FILE_ENV`] are still absent. In that half-set state the
/// inner build would be uninstrumented and the runner would emit no
/// `.profraw`, so this fails loudly rather than silently producing an empty
/// report.
///
/// # Errors
///
/// Fails when either [`RUSTC_WRAPPER_ENV`] or [`LLVM_PROFILE_FILE_ENV`] is
/// unset, naming the missing variable and the correct `--sh` invocation.
#[inline]
pub fn validate_wrapper_env() -> Result<()> {
    validate_wrapper(
        env::var_os(RUSTC_WRAPPER_ENV).as_deref(),
        env::var_os(LLVM_PROFILE_FILE_ENV).as_deref(),
    )
}

/// Pure predicate for [`validate_wrapper_env`]: both wrapper variables must
/// be present, else the environment is incomplete and the build would be
/// uninstrumented.
#[inline]
fn validate_wrapper(
    rustc_wrapper: Option<&OsStr>,
    llvm_profile_file: Option<&OsStr>,
) -> Result<()> {
    for (name, value) in [
        (RUSTC_WRAPPER_ENV, rustc_wrapper),
        (LLVM_PROFILE_FILE_ENV, llvm_profile_file),
    ] {
        if value.is_none() {
            anyhow::bail!(
                "`{COVERAGE_ENV}` is set but `{name}` is not: the coverage environment is \
                 incomplete. `cargo llvm-cov show-env` without `--sh` emits non-exported \
                 assignments, so `eval \"$(cargo llvm-cov show-env)\"` leaves them shell-local \
                 and child processes do not inherit them. Re-run with \
                 `eval \"$(cargo llvm-cov show-env --sh)\"` and eval its output before \
                 `cargo rudzio test`, or unset `{COVERAGE_ENV}` to run without coverage."
            );
        }
    }
    Ok(())
}

/// Build a single `--remap-path-prefix=<from>=<to>` rustflag.
///
/// Rendered as one whitespace-free token so it survives the space-split
/// cargo applies to the `RUSTFLAGS` environment variable. `--remap-path-prefix`
/// maps a bridge-local compiled source path (the copy/symlink under the
/// generated aggregator) back to the member's real source path, so coverage
/// hits attribute to the member's actual `src/` files.
///
/// # Errors
///
/// Fails when either path is not valid UTF-8: the flag carries plain string
/// arguments and a lossy conversion would silently point coverage at the
/// wrong file.
#[inline]
pub fn remap_prefix_flag(from: &Path, to: &Path) -> Result<String> {
    let from_str = from.to_str().ok_or_else(|| {
        anyhow::anyhow!("remap source path is not valid UTF-8: {}", from.display())
    })?;
    let to_str = to
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("remap target path is not valid UTF-8: {}", to.display()))?;
    Ok(format!("--remap-path-prefix={from_str}={to_str}"))
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
#[inline]
pub fn resolve_aggregator_dirs(
    coverage_active: bool,
    coverage_target_dir: Option<&OsStr>,
    coverage_build_dir: Option<&OsStr>,
    base_output_dir: &Path,
) -> Result<AggregatorDirs> {
    if !coverage_active {
        return Ok(AggregatorDirs {
            build_dir: Some(base_output_dir.join("build-dir")),
            target_dir: base_output_dir.join("target"),
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
        build_dir: coverage_build_dir.map(PathBuf::from),
        target_dir: PathBuf::from(target_dir),
    })
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
#[cfg(any(test, rudzio_test))]
mod tests {
    use super::{
        COVERAGE_TARGET_DIR_ENV, LLVM_PROFILE_FILE_ENV, RUSTC_WRAPPER_ENV, Test,
        augment_crate_names, remap_prefix_flag, resolve_aggregator_dirs, validate_wrapper,
    };
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    const BASE: &str = "/outer/target/rudzio-auto-runner-0123456789abcdef-0123456789abcdef";

    #[rudzio::test]
    async fn non_coverage_anchors_under_base_output_dir(_ctx: &Test) -> anyhow::Result<()> {
        let dirs = resolve_aggregator_dirs(false, None, None, Path::new(BASE))?;
        assert_eq!(
            dirs.target_dir,
            Path::new("/outer/target/rudzio-auto-runner-0123456789abcdef-0123456789abcdef/target"),
            "target dir is BASE + `target` suffix"
        );
        assert_eq!(
            dirs.build_dir.as_deref(),
            Some(Path::new(
                "/outer/target/rudzio-auto-runner-0123456789abcdef-0123456789abcdef/build-dir"
            )),
            "build dir is BASE + `build-dir` suffix"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn coverage_uses_llvm_cov_target_dir_even_when_it_conflicts_with_base(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        // The coverage target dir is the directory `cargo llvm-cov report`
        // searches; it has no relationship to `base_output_dir`. This is the
        // "conflicting CARGO_TARGET_DIR" case: the outer target dir (BASE's
        // ancestor) differs from the coverage target dir.
        let dirs = resolve_aggregator_dirs(
            true,
            Some(OsStr::new("/var/mnt/data-cache/cargo-target")),
            Some(OsStr::new("/var/mnt/data-cache/cargo/build-dir")),
            Path::new(BASE),
        )?;
        assert_eq!(
            dirs.target_dir,
            Path::new("/var/mnt/data-cache/cargo-target"),
            "coverage overrides the target dir with the llvm-cov one"
        );
        assert_eq!(
            dirs.build_dir.as_deref(),
            Some(Path::new("/var/mnt/data-cache/cargo/build-dir")),
            "coverage overrides the build dir with the llvm-cov one"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn coverage_without_build_dir_does_not_override_build_dir_env(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let dirs =
            resolve_aggregator_dirs(true, Some(OsStr::new("/cov-target")), None, Path::new(BASE))?;
        assert_eq!(
            dirs.target_dir,
            Path::new("/cov-target"),
            "coverage target dir wins"
        );
        assert_eq!(
            dirs.build_dir, None,
            "build dir stays unset when not provided"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn coverage_without_target_dir_fails_loudly(_ctx: &Test) -> anyhow::Result<()> {
        let Err(err) = resolve_aggregator_dirs(true, None, None, Path::new(BASE)) else {
            anyhow::bail!("expected coverage dir resolution to fail when target dir is unset");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains(COVERAGE_TARGET_DIR_ENV),
            "error should name the missing env var: {msg}"
        );
        assert!(
            msg.contains("incomplete"),
            "error should say the env is incomplete: {msg}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn augment_appends_generated_names_not_already_present(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let existing = "consumer,consumer_tests,consumer,main";
        let generated = vec!["rudzio_auto_runner".to_owned()];
        let augmented = augment_crate_names(existing, &generated);
        assert_eq!(
            augmented, "consumer,consumer_tests,main,rudzio_auto_runner",
            "existing duplicates collapsed, generated runner name appended"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn augment_is_idempotent_when_generated_already_present(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let existing = "consumer,rudzio_auto_runner";
        let generated = vec!["rudzio_auto_runner".to_owned()];
        assert_eq!(
            augment_crate_names(existing, &generated),
            "consumer,rudzio_auto_runner",
            "already-present generated name is not re-appended"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn augment_tolerates_empty_existing_and_trims(_ctx: &Test) -> anyhow::Result<()> {
        let generated = vec![
            "rudzio_auto_runner".to_owned(),
            "a_bin_rudzio_bridge_lib".to_owned(),
        ];
        assert_eq!(
            augment_crate_names("", &generated),
            "rudzio_auto_runner,a_bin_rudzio_bridge_lib",
            "empty existing yields just the generated names"
        );
        assert_eq!(
            augment_crate_names("  consumer ,, consumer ", &generated),
            "consumer,rudzio_auto_runner,a_bin_rudzio_bridge_lib",
            "whitespace and duplicates are trimmed and collapsed"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn remap_flag_is_one_whitespace_free_token(_ctx: &Test) -> anyhow::Result<()> {
        let flag = remap_prefix_flag(
            Path::new("/out/members/consumer"),
            Path::new("/ws/consumer"),
        )?;
        assert_eq!(
            flag, "--remap-path-prefix=/out/members/consumer=/ws/consumer",
            "flag is a single whitespace-free `--remap-path-prefix` token"
        );
        assert!(
            !flag.contains(' '),
            "must survive RUSTFLAGS whitespace split: {flag}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn remap_flag_errors_on_non_utf8_source(_ctx: &Test) -> anyhow::Result<()> {
        use std::os::unix::ffi::OsStringExt as _;
        let bad = PathBuf::from(OsString::from_vec(vec![0xff, 0xfe]));
        let Err(err) = remap_prefix_flag(&bad, Path::new("/ws/consumer")) else {
            anyhow::bail!("expected remap flag construction to fail on non-UTF-8 source");
        };
        assert!(
            format!("{err:#}").contains("not valid UTF-8"),
            "non-UTF-8 source path is rejected"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn validate_wrapper_accepts_both_variables(_ctx: &Test) -> anyhow::Result<()> {
        validate_wrapper(
            Some(OsStr::new("/bin/cargo-llvm-cov")),
            Some(OsStr::new("/target/ws-%p.profraw")),
        )
    }

    #[rudzio::test]
    async fn validate_wrapper_fails_on_missing_rustc_wrapper(_ctx: &Test) -> anyhow::Result<()> {
        let Err(err) = validate_wrapper(None, Some(OsStr::new("/target/ws-%p.profraw"))) else {
            anyhow::bail!("expected missing RUSTC_WRAPPER to fail validation");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains(RUSTC_WRAPPER_ENV),
            "should name RUSTC_WRAPPER: {msg}"
        );
        assert!(
            msg.contains("--sh"),
            "should recommend `show-env --sh`: {msg}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn validate_wrapper_fails_on_missing_llvm_profile_file(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let Err(err) = validate_wrapper(Some(OsStr::new("/bin/cargo-llvm-cov")), None) else {
            anyhow::bail!("expected missing LLVM_PROFILE_FILE to fail validation");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains(LLVM_PROFILE_FILE_ENV),
            "should name LLVM_PROFILE_FILE: {msg}"
        );
        Ok(())
    }
}
