//! End-to-end reproducer for migration Bug #3: `cargo rudzio test` run
//! under `cargo llvm-cov` instrumentation must produce a coverage report
//! that attributes non-zero line coverage to the member's real `src/`
//! files.
//!
//! # Documented flow (the exact three steps under test)
//!
//! ```text
//! eval "$(cargo llvm-cov show-env)"
//! cargo rudzio test
//! cargo llvm-cov report
//! ```
//!
//! # Symptom (pre-fix)
//!
//! Two independent failures, both surfaced by driving the flow above:
//!
//! 1. `cargo rudzio test` walks the `rudzio` path-dependency as if it were
//!    a workspace member and generates a bridge crate for the `rudzio`
//!    framework itself. `rudzio`'s manifest has an in-crate dogfood
//!    self-dev-dep (`rudzio = { path = "." }`) that satisfies the
//!    "declares a rudzio dependency" gate, so it is bridged — and its
//!    `anyhow = { workspace = true }` cannot resolve against the consumer
//!    root manifest, so `cargo rudzio test` exits 1 before any test runs.
//!
//! 2. Even when the bridge build succeeds, the bridge compiles the
//!    member's source from a bridge-local copy (`members/<member>/src/`
//!    under the aggregator), and the coverage mapping records that path
//!    *relative to the aggregator workspace root*. The
//!    `--remap-path-prefix` emitted for coverage used an ABSOLUTE bridge
//!    path, which never matches the relative coverage-mapping path, so
//!    `cargo llvm-cov report` attributes coverage to the transient copy
//!    (hidden by the default ignore-filename-regex) and reports 0 lines.
//!
//! # Assertion (RED before the fix, GREEN after)
//!
//! The three-step flow must complete with `cargo llvm-cov report` exiting
//! 0 AND non-zero line coverage attributed to the consumer's real
//! `src/lib.rs`. A bare exit-0 report is not enough: an empty report also
//! exits 0, so the spec pins the actual file attribution via `--json`.
//!
//! # Fresh build (no stale/foreign binary)
//!
//! Drives `cargo run -p cargo-rudzio -- test` (not
//! `env!("CARGO_BIN_EXE_cargo-rudzio")`) so the binary under test is a
//! fresh build into a task-local `CARGO_TARGET_DIR` / `CARGO_BUILD_BUILD_DIR`.
//!
//! # Missing toolchain fails loudly
//!
//! If `cargo llvm-cov` is not installed, or its backing `llvm-tools`
//! rustup component is missing (`show-env` fails), the test fails with a
//! message naming the missing tool and the fix (`nix develop`). No silent
//! skip.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::Context as _;
use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// Repo root = parent of `cargo-rudzio/` (the rudzio workspace). The
/// consumer's `rudzio` dev-dep points here by path, mirroring a real
/// consumer that path-depends on a checkout of the rudzio framework.
fn repo_root() -> &'static Path {
    // `CARGO_MANIFEST_DIR` is `<repo>/cargo-rudzio`; its parent is the repo
    // root. The fallback (the manifest dir itself) is unreachable, but keeps
    // the helper panic-free on a hypothetical path with no parent.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
}

/// Resolve the ambient cargo home that holds cargo-rudzio's own dep graph.
///
/// `cargo run -p cargo-rudzio -- test` rebuilds the binary from the repo
/// root, so it needs cargo-rudzio's full dependency graph — which lives in
/// the ambient `CARGO_HOME` populated by the test harness's own build, not
/// the consumer-scoped pre-warmed home.
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

/// Run `cargo llvm-cov show-env` from `workspace` under the per-test
/// isolated cargo state. `CARGO_TARGET_DIR` is pinned so the emitted
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
        .arg("--export-prefix")
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
/// The `--export-prefix` variant emits `export NAME='value'` lines; values
/// are single-quoted only when they contain characters that need shell
/// quoting, and bare otherwise. This preserves the raw value bytes —
/// including the unit-separator (`\x1f`) argument delimiter inside
/// `__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS` — so the subprocesses receive
/// exactly what `eval "$(cargo llvm-cov show-env)"` would set.
fn parse_show_env(stdout: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(stdout);
    let mut assignments = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line
            .trim_end_matches('\r')
            .strip_prefix("export ")
            .unwrap_or_else(|| raw_line.trim_end_matches('\r'));
        if line.is_empty() {
            continue;
        }
        let (name, raw_value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("unexpected show-env line without `=`: {line:?}"))?;
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

/// Drive `cargo run -p cargo-rudzio -- test` from the repo root: a fresh
/// build of the cargo-rudzio binary into task-local target/build dirs, then
/// the `test` subcommand against `workspace` with the coverage env applied.
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
/// build ran with. The JSON shape is what lets the test assert *which* file
/// carries line coverage instead of trusting a bare exit-0 report.
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
/// source from a bridge-local copy, so coverage points at the transient
/// aggregator path unless the fix remaps it back to the member's real
/// directory. A report that exits 0 but shows the consumer's `src/lib.rs`
/// at 0 covered lines (or omits it entirely) is the same bug in disguise,
/// so this assert pins the real-file attribution.
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

/// Rewrite the consumer member's `Cargo.toml` so `rudzio` is a DIRECT path
/// dependency on the repo root instead of a `workspace = true` entry.
///
/// The pre-warmed helper template uses `rudzio = { workspace = true }`,
/// which does NOT exercise the transitive path-dep walk that bridges the
/// `rudzio` framework itself (root cause #2). A direct `path = …` entry is
/// what a real checkout-of-rudzio consumer declares, and it is what makes
/// the bug reproduce.
fn rewrite_consumer_to_direct_path_dep(workspace: &Path) -> anyhow::Result<()> {
    let manifest = workspace.join("consumer").join("Cargo.toml");
    let contents = format!(
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\", \"runtime-tokio-current-thread\"] }}\n\
         anyhow = \"=1.0.104\"\n",
        rudzio = repo_root().display(),
    );
    fs::write(&manifest, contents)
        .with_context(|| format!("rewriting consumer Cargo.toml at {}", manifest.display()))?;
    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, assert_consumer_src_line_coverage, cargo_home_with_full_graph,
        llvm_cov_show_env, llvm_cov_version_probe, parse_show_env,
        rewrite_consumer_to_direct_path_dep, run_cargo_rudzio_test, run_llvm_cov_report,
        run_llvm_cov_report_json,
    };

    /// **Migration Bug #3** — `cargo rudzio test` under coverage must emit
    /// coverage attributed to the member's real `src/`, and `cargo llvm-cov
    /// report` must exit 0 with non-zero covered lines.
    #[rudzio::test]
    async fn cargo_rudzio_test_emits_profraw_into_llvm_cov_search_dir(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        // Fail loudly when the coverage toolchain is absent.
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

        let workspace: &Path = ctx.consumer_workspace();
        // Faithful repro: direct rudzio path-dep (see the rewrite helper).
        rewrite_consumer_to_direct_path_dep(workspace)?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        // Fail loudly when `cargo llvm-cov` is present but its backing
        // `llvm-tools` rustup component is missing: `show-env` cannot run
        // and `report` would fail for an unrelated reason.
        let show_env = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir())?;
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
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            workspace,
            &coverage_env,
        )?;
        anyhow::ensure!(
            test_output.status.success(),
            "`cargo rudzio test` under coverage instrumentation exited {:?}; \
             expected success before the report step. Bug #3: the `rudzio` path-dep was \
             walked as a member and its bridge failed to resolve `anyhow = {{ workspace \
             = true }}` against the consumer root.\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            test_output.status.code(),
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );

        let report_output = run_llvm_cov_report(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &coverage_env,
        )?;
        anyhow::ensure!(
            report_output.status.success(),
            "`cargo llvm-cov report` exited {:?}; expected success.\n\
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
            &cargo_home,
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
