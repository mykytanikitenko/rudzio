//! End-to-end reproducer for migration Bug #3: `cargo rudzio test` run
//! under `cargo llvm-cov` instrumentation writes its `.profraw` coverage
//! files — and the instrumented object files llvm-cov needs to merge them —
//! into the aggregator's private target tree instead of the directory
//! `cargo llvm-cov report` searches.
//!
//! # Symptom (pre-fix)
//!
//! ```text
//! eval "$(cargo llvm-cov show-env)"
//! cargo rudzio test
//! cargo llvm-cov report
//! # error: failed to collect object files: not found object files
//! # (searched directory: <target>/llvm-cov-target/debug)
//! ```
//!
//! # Cause sketch (informs the assertion)
//!
//! `cargo rudzio test` generates an aggregator workspace and builds it with
//! an inner `cargo` whose `CARGO_TARGET_DIR` is
//! `<target>/rudzio-auto-runner-<hash>/target`. The `-Cinstrument-coverage`
//! objects therefore land in the auto-runner target tree, not in the
//! `llvm-cov-target` tree that `cargo llvm-cov report` scans, so the report
//! step fails with "not found object files".
//!
//! # Assertion
//!
//! Behavioural: drive the three-step repro against an isolated single-lib
//! consumer workspace carrying one `#[rudzio::test]`. The environment
//! variables emitted by `cargo llvm-cov show-env` are applied to both
//! spawned subprocesses, and `cargo llvm-cov report` is required to exit 0.
//! Under the current code the report step fails, so this test is red until
//! the fix lands.
//!
//! # Missing toolchain fails loudly
//!
//! The coverage toolchain is required for this spec. If `cargo llvm-cov` is
//! not installed (`cargo llvm-cov --version` fails) or its `show-env` probe
//! fails (the `llvm-tools` rustup component is missing from the active
//! toolchain), the test fails with a message naming the missing tool and the
//! fix: enter `nix develop`, which provides cargo-llvm-cov and the
//! llvm-tools rustup component. There is no silent skip.

use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// Probe whether the `cargo llvm-cov` subcommand is installed.
///
/// Exit status 0 means the subcommand resolves and can be driven; any
/// non-zero status or spawn failure means the coverage toolchain is absent
/// and the test must skip.
fn llvm_cov_version_probe() -> io::Result<Output> {
    Command::new("cargo")
        .arg("llvm-cov")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Run `cargo llvm-cov show-env` from `workspace` under the per-test
/// isolated cargo state.
///
/// `CARGO_TARGET_DIR` is pinned to the per-test target dir so the emitted
/// `LLVM_PROFILE_FILE` / `CARGO_LLVM_COV_TARGET_DIR` point inside the
/// ephemeral per-test tree rather than the host's shared target dir.
fn llvm_cov_show_env(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
) -> io::Result<Output> {
    Command::new("cargo")
        .arg("llvm-cov")
        .arg("show-env")
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Parse `cargo llvm-cov show-env` stdout into `(name, value)` pairs.
///
/// The default format is one `NAME='value'` assignment per line (values are
/// single-quoted only when they contain characters that need shell quoting,
/// and bare otherwise). This preserves the raw value bytes — including the
/// unit-separator (`\x1f`) argument delimiter inside
/// `__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS` — so the subprocesses receive
/// exactly what `eval "$(cargo llvm-cov show-env)"` would set.
fn parse_show_env(stdout: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(stdout);
    let mut assignments = Vec::new();
    for raw_line in text.lines() {
        let cr_stripped = raw_line.trim_end_matches('\r');
        // Tolerate both the bare default format and the `--export-prefix`
        // (`export NAME='value'`) variant without branching on a flag.
        let line = cr_stripped.strip_prefix("export ").unwrap_or(cr_stripped);
        if line.is_empty() {
            continue;
        }
        let (name, raw_value) = line.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("unexpected `cargo llvm-cov show-env` line without `=`: {line:?}")
        })?;
        assignments.push((name.to_owned(), unquote(raw_value).to_owned()));
    }
    Ok(assignments)
}

/// Strip a single pair of surrounding single quotes from a show-env value.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
        .unwrap_or(value)
}

/// Spawn `cargo-rudzio test` (the `cargo rudzio test` binary) against
/// `workspace` under the per-test isolated cargo state, plus the coverage
/// environment emitted by `cargo llvm-cov show-env`.
fn run_cargo_rudzio_test(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    coverage_env: &[(String, String)],
) -> io::Result<Output> {
    Command::new(
        cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .map_err(io::Error::other)?,
    )
    .arg("test")
    .current_dir(workspace)
    .env("CARGO_HOME", cargo_home)
    .env("CARGO_TARGET_DIR", cargo_target_dir)
    .env("CARGO_NET_OFFLINE", "true")
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

/// Spawn `cargo llvm-cov report` against `workspace` under the per-test
/// isolated cargo state, plus the same coverage environment the build ran
/// with.
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
        .env("CARGO_NET_OFFLINE", "true")
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

/// Spawn `cargo llvm-cov report --json` against `workspace` under the
/// per-test isolated cargo state, plus the same coverage environment the
/// build ran with. The JSON (`llvm.coverage.json.export`) shape is what
/// lets the test assert *which* file carries line coverage instead of
/// trusting a bare exit-0 report.
fn run_llvm_cov_report_json(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    coverage_env: &[(String, String)],
) -> io::Result<Output> {
    Command::new("cargo")
        .arg("llvm-cov")
        .arg("report")
        .arg("--json")
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
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

/// Assert that the `cargo llvm-cov report --json` payload attributes
/// **non-zero line coverage** to the consumer's real `src/lib.rs`.
///
/// The root cause under test is that the aggregator compiles the member's
/// source from a bridge-local copy (and symlinked sibling modules), so
/// coverage points at the transient aggregator path unless the fix remaps
/// it back to the member's real directory. A report that exits 0 but shows
/// the consumer's `src/lib.rs` at 0 covered lines (or omits it entirely) is
/// the same bug in disguise, so this assert pins the real-file attribution.
fn assert_consumer_src_line_coverage(report_json: &str) -> anyhow::Result<()> {
    let report: serde_json::Value = serde_json::from_str(report_json)
        .map_err(|err| anyhow::anyhow!("failed to parse `cargo llvm-cov report --json`: {err}"))?;
    let files = report
        .pointer("/data/0/files")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("unexpected `--json` shape (no /data/0/files array): {report_json}")
        })?;
    let consumer_lib = files.iter().find(|file| {
        file.get("filename")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|filename| filename.ends_with("consumer/src/lib.rs"))
    });
    let covered = consumer_lib
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`cargo llvm-cov report --json` did not list consumer/src/lib.rs; \
                 coverage was not attributed to the member's real source. Files:\n\
                 {report_json}"
            )
        })?
        .pointer("/summary/lines/covered")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!("consumer/src/lib.rs entry had no /summary/lines/covered")
        })?;
    anyhow::ensure!(
        covered > 0,
        "consumer/src/lib.rs reported 0 covered lines; the trivial test should have \
         executed at least its own body"
    );
    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, assert_consumer_src_line_coverage, llvm_cov_show_env,
        llvm_cov_version_probe, parse_show_env, run_cargo_rudzio_test, run_llvm_cov_report,
        run_llvm_cov_report_json,
    };

    /// **Migration Bug #3** — `cargo rudzio test` must emit `.profraw`
    /// coverage into the directory `cargo llvm-cov report` searches.
    #[rudzio::test]
    async fn cargo_rudzio_test_emits_profraw_into_llvm_cov_search_dir(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        // Fail loudly when the coverage toolchain is absent: `cargo llvm-cov`
        // does not resolve on this machine.
        let version = llvm_cov_version_probe()?;
        if !version.status.success() {
            anyhow::bail!(
                "`cargo llvm-cov` is not installed: `cargo llvm-cov --version` failed with \
                 {:?}. Enter `nix develop`, which provides cargo-llvm-cov and the \
                 llvm-tools rustup component.\n--- stdout ---\n{}\n--- stderr ---\n{}",
                version.status.code(),
                String::from_utf8_lossy(&version.stdout),
                String::from_utf8_lossy(&version.stderr),
            );
        }

        // The per-test consumer workspace already ships one trivial
        // `#[rudzio::test]`, which is all the aggregator build needs.
        let workspace: &Path = ctx.consumer_workspace();

        // Fail loudly when `cargo llvm-cov` is present but its backing
        // `llvm-tools` rustup component (llvm-cov / llvm-profdata) is not
        // installed for the active toolchain: `show-env` cannot run and
        // `report` would fail for an unrelated reason, not the bug under test.
        let show_env = llvm_cov_show_env(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
        if !show_env.status.success() {
            anyhow::bail!(
                "`cargo llvm-cov show-env` failed (is the `llvm-tools` rustup component \
                 missing?): exited {:?}. Enter `nix develop`, which provides cargo-llvm-cov \
                 and the llvm-tools rustup component.\n--- stdout ---\n{}\n--- stderr ---\n{}",
                show_env.status.code(),
                String::from_utf8_lossy(&show_env.stdout),
                String::from_utf8_lossy(&show_env.stderr),
            );
        }
        let coverage_env = parse_show_env(&show_env.stdout)?;

        let test_output = run_cargo_rudzio_test(
            workspace,
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &coverage_env,
        )?;
        anyhow::ensure!(
            test_output.status.success(),
            "`cargo rudzio test` under coverage instrumentation exited {:?}; \
             expected success before the report step.\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            test_output.status.code(),
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );

        let report_output = run_llvm_cov_report(
            workspace,
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &coverage_env,
        )?;
        anyhow::ensure!(
            report_output.status.success(),
            "`cargo llvm-cov report` exited {:?}; expected success. Bug #3: the \
             aggregator built into its own `rudzio-auto-runner-<hash>/target` tree, so \
             no object files exist where llvm-cov searches (`<target>/llvm-cov-target/debug`).\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            report_output.status.code(),
            String::from_utf8_lossy(&report_output.stdout),
            String::from_utf8_lossy(&report_output.stderr),
        );

        // Bare exit 0 is not enough: the report must attribute non-zero line
        // coverage to the consumer's REAL `src/lib.rs`, not the transient
        // aggregator copy (which the default ignore-filename-regex hides).
        let json_output = run_llvm_cov_report_json(
            workspace,
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &coverage_env,
        )?;
        anyhow::ensure!(
            json_output.status.success(),
            "`cargo llvm-cov report --json` exited {:?}.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            json_output.status.code(),
            String::from_utf8_lossy(&json_output.stdout),
            String::from_utf8_lossy(&json_output.stderr),
        );
        assert_consumer_src_line_coverage(&String::from_utf8_lossy(&json_output.stdout))?;

        Ok(())
    }
}
