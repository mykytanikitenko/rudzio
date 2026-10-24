//! End-to-end reproducer for migration Bug #3 against the operator's
//! **exact** `token` crate shape (not the synthetic `consumer` crate used
//! by [`super::migration_bug_03_coverage_profraw_spec`]).
//!
//! # Why a second Bug #3 spec
//!
//! The existing Bug #3 spec drives a minimal `consumer` lib whose `rudzio`
//! entry is rewritten to a **direct path dependency**. The operator's real
//! crate is shaped differently, and the earlier fix did not account for
//! those differences. This spec reproduces the exact shape:
//!
//! * a workspace rooted at `members = ["crates/*"]`, resolver `2`;
//! * `rudzio` declared as a **direct git dependency** (with a real `rev`
//!   pin and `runtime-futures`/`runtime-compio`/`common` features) in
//!   `[workspace.dependencies]`, inherited by the member via
//!   `rudzio.workspace = true`;
//! * a consumer `.cargo/config.toml` that sets `build.build-dir` to a
//!   separate `tmp/build-dir`;
//! * an externally-pinned `CARGO_TARGET_DIR` (the per-test isolated
//!   target dir stands in for the operator's external target dir);
//! * `[profile.dev]` and `[profile.release]` both `panic = "abort"`;
//! * a lib-only `token` member with no `[[bin]]` and no `tests/` dir,
//!   whose inline `#[rudzio::suite]` lives in `src/asm/backend.rs` under a
//!   `futures::ThreadPool` runtime and holds three real tests against
//!   `Backend::from_name`.
//!
//! # Documented flow (the exact three steps under test)
//!
//! ```text
//! eval "$(cargo llvm-cov show-env)"
//! cargo rudzio test -p token
//! cargo llvm-cov report
//! ```
//!
//! # Assertion (RED before the fix, GREEN after)
//!
//! Two independent assertions, both required:
//!
//! 1. at least one `*.profraw` file exists after `cargo rudzio test`
//!    under the `show-env` contract; and
//! 2. `cargo llvm-cov report` exits 0 AND attributes non-zero line
//!    coverage to the token member's real `src/asm/backend.rs`.
//!
//! A bare exit-0 report is not enough: an empty report also exits 0, and a
//! report that attributes coverage to the transient aggregator copy (or to
//! zero covered lines) is the same bug in disguise. The `.profraw`-count
//! assertion pins the *instrumentation + profile-write* half of the bug;
//! the JSON line-coverage assertion pins the *attribution* half.
//!
//! # Fresh build (no stale/foreign binary)
//!
//! Drives `cargo run -p cargo-rudzio -- test` (not
//! `env!("CARGO_BIN_EXE_cargo-rudzio")`) so the binary under test is a
//! fresh build into a task-local `CARGO_TARGET_DIR` /
//! `CARGO_BUILD_BUILD_DIR`.
//!
//! # Missing toolchain fails loudly
//!
//! If `cargo llvm-cov` is not installed, or its backing `llvm-tools`
//! component is missing (`show-env` fails), the test fails with a message
//! naming the missing tool and the fix. No silent skip.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::Context as _;
use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread;

/// Repo root = parent of `cargo-rudzio/` (the rudzio workspace). The
/// consumer's `rudzio` git dep points here by `file://` URL.
fn rudzio_repo_root() -> PathBuf {
    repo_root()
}

/// Absolute `file://` git URL for the rudzio repo this test lives in,
/// mirroring the operator's `git = "file://<abs repo root>"` declaration.
fn rudzio_git_url() -> String {
    format!("file://{}", rudzio_repo_root().display())
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
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Parse `cargo llvm-cov show-env --export-prefix` stdout into
/// `(name, value)` pairs.
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
        .current_dir(rudzio_repo_root())
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
/// per-test isolated cargo state, plus the same coverage environment.
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

/// Count `*.profraw` files directly under `cargo_target_dir` (the
/// non-recursive location `cargo llvm-cov report` searches).
fn count_profraw(cargo_target_dir: &Path) -> io::Result<usize> {
    let mut count = 0_usize;
    for entry_result in fs::read_dir(cargo_target_dir)? {
        let dir_entry = entry_result?;
        let raw_name = dir_entry.file_name();
        let entry_name = raw_name.to_string_lossy();
        if entry_name.ends_with(".profraw") {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// Rewrite the pre-warmed `consumer` workspace into the operator's exact
/// `token` workspace shape.
///
/// The workspace root gains `members = ["crates/*"]`, resolver 2, a
/// `[workspace.dependencies]` table carrying `anyhow`, `thiserror`, and a
/// direct git `rudzio` dep (with real `rev` + the operator's feature set),
/// `[workspace.lints.rust]` denying warnings/unsafe/unexpected-cfgs, and
/// `panic = "abort"` profiles. The member becomes a lib-only `token` crate
/// whose inline suite lives in `src/asm/backend.rs`.
fn write_token_fixture(workspace: &Path, git_url: &str, rev: &str) -> anyhow::Result<()> {
    // Remove the pre-warmed `consumer` member and any stale manifest.
    let consumer = workspace.join("consumer");
    if consumer.exists() {
        fs::remove_dir_all(&consumer)
            .with_context(|| format!("removing pre-warmed consumer at {}", consumer.display()))?;
    }
    let old_manifest = workspace.join("Cargo.toml");
    if old_manifest.exists() {
        fs::remove_file(&old_manifest)?;
    }

    // Workspace root manifest: members under crates/*, resolver 2, the
    // operator's workspace.dependencies (including the direct git rudzio
    // dep), lint gates, and panic=abort profiles.
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"crates/*\"]\n\
             resolver = \"2\"\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.dependencies]\n\
             anyhow = \"=1.0.104\"\n\
             thiserror = \"=2.0.21\"\n\
             rudzio = {{ git = \"{git_url}\", rev = \"{rev}\", features = [\"runtime-futures\", \"runtime-compio\", \"common\"] }}\n\
             \n\
             [workspace.lints.rust]\n\
             warnings = \"deny\"\n\
             unsafe_code = \"deny\"\n\
             unexpected_cfgs = {{ level = \"deny\", check-cfg = [\"cfg(rudzio_test)\"] }}\n\
             \n\
             [profile.dev]\n\
             panic = \"abort\"\n\
             \n\
             [profile.release]\n\
             panic = \"abort\"\n"
        ),
    )
    .with_context(|| format!("writing workspace manifest at {}", workspace.display()))?;

    // Consumer `.cargo/config.toml` with a separate build-dir.
    let cargo_config = workspace.join(".cargo").join("config.toml");
    let cargo_config_parent = cargo_config
        .parent()
        .context("workspace `.cargo` dir has a parent")?;
    fs::create_dir_all(cargo_config_parent)?;
    fs::write(&cargo_config, "[build]\nbuild-dir = \"tmp/build-dir\"\n")?;

    // Member manifest: lib-only, edition inherited, thiserror + rudzio.
    let token = workspace.join("crates").join("token");
    fs::create_dir_all(token.join("src").join("asm"))?;
    fs::write(
        token.join("Cargo.toml"),
        "[package]\n\
         name = \"token\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         publish = false\n\
         \n\
         [dependencies]\n\
         thiserror.workspace = true\n\
         \n\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n",
    )?;

    // lib.rs: clean-module declarations only.
    fs::write(
        token.join("src").join("lib.rs"),
        "pub mod asm;\npub mod span;\npub mod token;\n",
    )?;
    fs::write(token.join("src").join("asm.rs"), "pub mod backend;\n")?;
    fs::write(
        token.join("src").join("span.rs"),
        "/// Byte span.\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub struct Span { pub start: usize, pub end: usize }\n",
    )?;
    fs::write(
        token.join("src").join("token.rs"),
        "/// Token kind.\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Token { Ident, Number }\n",
    )?;

    // Inline suite at the end of src/asm/backend.rs: three real tests on
    // `Backend::from_name`, under a futures::ThreadPool runtime.
    fs::write(
        token.join("src").join("asm").join("backend.rs"),
        r#"/// Error for an unknown backend name.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown backend `{0}`")]
pub struct UnknownBackend(pub String);

/// Assembly backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend { X86, Arm }

impl Backend {
    /// Parse a backend by name.
    pub fn from_name(name: &str) -> Result<Self, UnknownBackend> {
        match name {
            "x86" => Ok(Self::X86),
            "arm" => Ok(Self::Arm),
            other => Err(UnknownBackend(other.to_owned())),
        }
    }
}

#[cfg(any(test, rudzio_test))]
#[rudzio::suite([
    (runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test),
])]
mod tests {
    use super::*;

    #[rudzio::test]
    fn parses_x86(_t: &rudzio::common::context::Test) -> anyhow::Result<()> {
        anyhow::ensure!(Backend::from_name("x86") == Ok(Backend::X86));
        Ok(())
    }

    #[rudzio::test]
    fn parses_arm(_t: &rudzio::common::context::Test) -> anyhow::Result<()> {
        anyhow::ensure!(Backend::from_name("arm") == Ok(Backend::Arm));
        Ok(())
    }

    #[rudzio::test]
    fn rejects_unknown(_t: &rudzio::common::context::Test) -> anyhow::Result<()> {
        anyhow::ensure!(Backend::from_name("mips") == Err(UnknownBackend("mips".to_owned())));
        Ok(())
    }
}
"#,
    )?;

    Ok(())
}

/// Assert that the `cargo llvm-cov report --json` payload attributes
/// **non-zero line coverage** to the token member's real `src/asm/backend.rs`.
fn assert_token_src_line_coverage(report_json: &str) -> anyhow::Result<()> {
    let report: serde_json::Value = serde_json::from_str(report_json)
        .map_err(|err| anyhow::anyhow!("failed to parse `cargo llvm-cov report --json`: {err}"))?;
    let files = report
        .pointer("/data/0/files")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("unexpected `--json` shape (no /data/0/files array): {report_json}")
        })?;
    let backend = files.iter().find(|file| {
        file.get("filename")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|filename| filename.ends_with("crates/token/src/asm/backend.rs"))
    });
    let covered = backend
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`cargo llvm-cov report --json` did not list token/src/asm/backend.rs; \
                 coverage was not attributed to the member's real source. Files:\n\
                 {report_json}"
            )
        })?
        .pointer("/summary/lines/covered")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!("token/src/asm/backend.rs entry had no /summary/lines/covered")
        })?;
    anyhow::ensure!(
        covered > 0,
        "token/src/asm/backend.rs reported 0 covered lines; the three `Backend::from_name` \
         tests should have executed their own bodies"
    );
    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, assert_token_src_line_coverage, cargo_home_with_full_graph,
        count_profraw, current_git_head, llvm_cov_show_env, llvm_cov_version_probe, parse_show_env,
        rudzio_git_url, run_cargo_rudzio_test, run_llvm_cov_report, run_llvm_cov_report_json,
        write_token_fixture,
    };

    /// **Migration Bug #3 (exact token shape)** — `cargo rudzio test` under
    /// coverage must emit at least one `.profraw` and `cargo llvm-cov
    /// report` must attribute non-zero line coverage to `src/asm/backend.rs`.
    #[rudzio::test]
    async fn cargo_rudzio_test_emits_profraw_for_exact_token_shape(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
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

        let rev = current_git_head()
            .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))?;
        let git_url = rudzio_git_url();
        write_token_fixture(ctx.consumer_workspace(), &git_url, &rev)?;

        let workspace: &Path = ctx.consumer_workspace();
        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        // Fail loudly when the coverage toolchain is present but its backing
        // `llvm-tools` component is missing: `show-env` cannot run and
        // `report` would fail for an unrelated reason.
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
             expected success before the report step.\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            test_output.status.code(),
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );

        // Instrumentation + profile-write half of the bug: at least one
        // `.profraw` must exist where `cargo llvm-cov report` searches.
        let profraw_count = count_profraw(ctx.cargo_target_dir())?;
        anyhow::ensure!(
            profraw_count > 0,
            "`cargo rudzio test` under coverage produced ZERO *.profraw files in {}; \
             the instrumented runner did not dump its profile.",
            ctx.cargo_target_dir().display(),
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

        // Attribution half of the bug: the report must attribute non-zero
        // line coverage to the token member's REAL `src/asm/backend.rs`.
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
        assert_token_src_line_coverage(&String::from_utf8_lossy(&json_output.stdout))?;

        Ok(())
    }
}
