//! Ground-truth e2e reproducer for Bug #3: `cargo rudzio test` under
//! `cargo llvm-cov` instrumentation does not emit `.profraw` files (and the
//! instrumented object files) into the directory `cargo llvm-cov report`
//! searches, so the report step fails with "not found object files".
//!
//! # Symptom (observed, fresh binary)
//!
//! ```text
//! eval "$(cargo llvm-cov show-env --export-prefix)"
//! cargo run -p cargo-rudzio -- test -p consumer
//! cargo llvm-cov report
//! # error: failed to collect object files: not found object files
//! # (searched directory: <target>/llvm-cov-target/debug)
//! ```
//!
//! The aggregator build anchors its inner `CARGO_TARGET_DIR` at
//! `<target>/rudzio-auto-runner-<hash>/target` instead of
//! `$CARGO_LLVM_COV_TARGET_DIR`, so the `-Cinstrument-coverage` objects land
//! outside the tree `report` scans.
//!
//! # Assertion (flips GREEN once fixed)
//!
//! The three-step flow must complete with `cargo llvm-cov report` exiting 0.
//!
//! # Fresh build (no stale binary)
//!
//! Drives `cargo run -p cargo-rudzio -- test` (not
//! `env!("CARGO_BIN_EXE_cargo-rudzio")`) with `RUSTC_WRAPPER=` (but the
//! llvm-cov wrapper from `show-env` is applied AFTER the fresh rebuild), plus
//! task-local `CARGO_TARGET_DIR` / `CARGO_BUILD_BUILD_DIR`.

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// Repo root = parent of `cargo-rudzio/` (the rudzio workspace).
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cargo-rudzio/tests must live under the repo root")
}

/// Resolve the ambient cargo home that holds cargo-rudzio's own dep graph.
fn cargo_home_with_full_graph() -> PathBuf {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// Probe whether `cargo llvm-cov` resolves. Any failure means the coverage
/// toolchain is absent and the test must fail loudly (no silent skip).
fn llvm_cov_version_probe() -> io::Result<Output> {
    Command::new("cargo")
        .arg("llvm-cov")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Run `cargo llvm-cov show-env --export-prefix` from `workspace`.
fn llvm_cov_show_env(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
) -> io::Result<Output> {
    Command::new("cargo")
        .arg("llvm-cov")
        .arg("show-env")
        .arg("--export-prefix")
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Parse `cargo llvm-cov show-env` stdout into `(name, value)` pairs.
fn parse_show_env(stdout: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(stdout);
    let mut assignments = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line
            .trim_end_matches('\r')
            .strip_prefix("export ")
            .unwrap_or(raw_line.trim_end_matches('\r'));
        if line.is_empty() {
            continue;
        }
        let (name, raw_value) = line.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("unexpected show-env line without `=`: {line:?}")
        })?;
        assignments.push((name.to_owned(), unquote(raw_value).to_owned()));
    }
    Ok(assignments)
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
        .unwrap_or(value)
}

/// Drive `cargo run -p cargo-rudzio -- test` from the repo root with the
/// coverage env applied and a fresh build into task-local target/build dirs.
fn run_cargo_rudzio_test(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    workspace: &Path,
    coverage_env: &[(String, String)],
) -> io::Result<Output> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Command::new(cargo_bin)
        .arg("run")
        .arg("-p")
        .arg("cargo-rudzio")
        .arg("--")
        .arg("test")
        .arg("--manifest-path")
        .arg(workspace.join("Cargo.toml"))
        .current_dir(repo_root())
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .envs(
            coverage_env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Run `cargo llvm-cov report` against `workspace` with the same coverage env.
fn run_llvm_cov_report(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    coverage_env: &[(String, String)],
) -> io::Result<Output> {
    Command::new("cargo")
        .arg("llvm-cov")
        .arg("report")
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .envs(
            coverage_env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, cargo_home_with_full_graph, llvm_cov_show_env,
        llvm_cov_version_probe, parse_show_env, repo_root, run_cargo_rudzio_test,
        run_llvm_cov_report,
    };

    /// **Bug #3** — `cargo rudzio test` under coverage must emit `.profraw`
    /// into the dir `cargo llvm-cov report` searches.
    #[rudzio::test]
    async fn coverage_report_finds_profraw(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let version = llvm_cov_version_probe()?;
        if !version.status.success() {
            anyhow::bail!(
                "`cargo llvm-cov` not installed: `cargo llvm-cov --version` failed {:?}.\n--- stdout ---\n{}\n--- stderr ---\n{}",
                version.status.code(),
                String::from_utf8_lossy(&version.stdout),
                String::from_utf8_lossy(&version.stderr),
            );
        }

        let workspace: &Path = ctx.consumer_workspace();
        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .expect("cargo target dir has a parent")
            .join("cargo-build-dir");

        let show_env = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir())?;
        if !show_env.status.success() {
            anyhow::bail!(
                "`cargo llvm-cov show-env` failed {:?} (llvm-tools component missing?).\n--- stdout ---\n{}\n--- stderr ---\n{}",
                show_env.status.code(),
                String::from_utf8_lossy(&show_env.stdout),
                String::from_utf8_lossy(&show_env.stderr),
            );
        }
        let coverage_env = parse_show_env(&show_env.stdout)?;

        let test_output = run_cargo_rudzio_test(
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            workspace,
            &coverage_env,
        )?;
        anyhow::ensure!(
            test_output.status.success(),
            "`cargo rudzio test` under coverage exited {:?}.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            test_output.status.code(),
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );

        let report_output =
            run_llvm_cov_report(workspace, &cargo_home, ctx.cargo_target_dir(), &coverage_env)?;
        anyhow::ensure!(
            report_output.status.success(),
            "`cargo llvm-cov report` exited {:?}. Bug #3: the aggregator built into its \
             own `rudzio-auto-runner-<hash>/target` tree, so no object files exist where \
             llvm-cov searches.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            report_output.status.code(),
            String::from_utf8_lossy(&report_output.stdout),
            String::from_utf8_lossy(&report_output.stderr),
        );

        let _ = repo_root();
        Ok(())
    }
}
