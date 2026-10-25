//! End-to-end reproducer for migration Bug #3 against the operator's
//! **bash + `eval "$(cargo llvm-cov show-env)"`** environment, not the
//! earlier "fish eval" theory (which the operator refuted: their shell is
//! bash and the coverage variables are set the way `show-env` emits them).
//!
//! # Why a third Bug #3 spec
//!
//! [`super::migration_bug_03_token_exact_e2e_spec`] drives the same `token`
//! crate shape but sources the coverage environment from
//! `cargo llvm-cov show-env --export-prefix`, i.e. it *forces* the
//! `export`-prefixed output. The operator's documented three-step flow
//! (Justfile `coverage` recipe and `docs/rudzio-bugs.md`) uses the bare
//! `cargo llvm-cov show-env` form:
//!
//! ```text
//! eval "$(cargo llvm-cov show-env)"
//! cargo rudzio test -p token
//! cargo llvm-cov report
//! ```
//!
//! Since cargo-llvm-cov 0.8.x the bare `show-env` output is a single line
//! of `NAME='value'` assignments (no `export ` prefix). `eval`-ing that in
//! bash/zsh creates **shell-local** variables that are **not exported** to
//! child processes, so `cargo rudzio test`'s inner cargo never sees
//! `RUSTC_WRAPPER` / `LLVM_PROFILE_FILE`, builds an uninstrumented runner,
//! writes zero `.profraw` files, and `cargo llvm-cov report` (whose
//! `CARGO_LLVM_COV_TARGET_DIR` is likewise absent) falls back to
//! `<target>/llvm-cov-target` and fails:
//!
//! ```text
//! error: failed to merge profile data: not found *.profraw files in <target>/llvm-cov-target
//! ```
//!
//! This spec pins two halves of that failure so neither can silently
//! regress:
//!
//! 1. **the `--sh` flag is load-bearing** — `show-env --sh` must emit
//!    `export NAME='value'` lines (the only form `eval` propagates), while
//!    the bare form must not, and under `--sh` the operator's exact
//!    workspace shape yields `.profraw` + non-zero token line coverage; and
//! 2. **partial coverage env must fail loudly** — when `CARGO_LLVM_COV` is
//!    present but the wrapper/profile-file variables are missing (exactly
//!    what a bare `eval` produces inside a half-set shell), `cargo rudzio
//!    test` must error, not silently emit zero `.profraw`.
//!
//! # Missing toolchain fails loudly
//!
//! If `cargo llvm-cov` is not installed (or its `llvm-tools` component is
//! missing), the test fails naming the missing tool. No silent skip.

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

/// Absolute `file://` git URL for the rudzio repo this test lives in.
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

/// Probe whether `cargo llvm-cov` resolves. Failure = missing toolchain.
fn llvm_cov_version_probe() -> io::Result<Output> {
    Command::new("cargo")
        .arg("llvm-cov")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Run `cargo llvm-cov show-env [flag]` from `workspace`. `flag` is either
/// the empty string (the operator's bare form) or `--sh`.
fn llvm_cov_show_env(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    flag: &str,
) -> io::Result<Output> {
    let mut args = vec!["llvm-cov", "show-env"];
    if !flag.is_empty() {
        args.push(flag);
    }
    let mut cmd = Command::new("cargo");
    cmd.args(&args)
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Parse `cargo llvm-cov show-env --sh` stdout into `(name, value)` pairs.
/// Each `--sh` line is `export NAME=value`; values are single-quoted when
/// they contain shell metacharacters and bare otherwise, so quote-stripping
/// is applied only when a surrounding `'` pair is present.
fn parse_show_env(stdout: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(stdout);
    let mut assignments = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let stripped = line.strip_prefix("export ").unwrap_or(line);
        let (name, value) = stripped
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("unexpected show-env line without `=`: {line:?}"))?;
        let unquoted = value
            .strip_prefix('\'')
            .and_then(|inner| inner.strip_suffix('\''))
            .unwrap_or(value);
        assignments.push((name.trim().to_owned(), unquoted.to_owned()));
    }
    Ok(assignments)
}

/// Drive `cargo run -p cargo-rudzio -- test` from the repo root with a
/// fresh build into task-local target/build dirs and the coverage env
/// applied.
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

/// Run `cargo llvm-cov report` against `workspace` with the coverage env.
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

/// Run `cargo llvm-cov report --json` against `workspace`.
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
        if dir_entry
            .file_name()
            .to_string_lossy()
            .ends_with(".profraw")
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// Rewrite the pre-warmed `consumer` workspace into the operator's exact
/// `token` workspace shape (mirrors
/// [`super::migration_bug_03_token_exact_e2e_spec::write_token_fixture`]).
fn write_token_fixture(workspace: &Path, git_url: &str, rev: &str) -> anyhow::Result<()> {
    let consumer = workspace.join("consumer");
    if consumer.exists() {
        fs::remove_dir_all(&consumer)
            .with_context(|| format!("removing pre-warmed consumer at {}", consumer.display()))?;
    }
    let old_manifest = workspace.join("Cargo.toml");
    if old_manifest.exists() {
        fs::remove_file(&old_manifest)?;
    }

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

    let cargo_config = workspace.join(".cargo").join("config.toml");
    let cargo_config_parent = cargo_config
        .parent()
        .context("workspace `.cargo` dir has a parent")?;
    fs::create_dir_all(cargo_config_parent)?;
    fs::write(&cargo_config, "[build]\nbuild-dir = \"tmp/build-dir\"\n")?;

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

    /// The bare `show-env` output is NOT `export`-prefixed, so `eval` makes
    /// shell-local (non-exported) variables; `--sh` is the only form that
    /// propagates to children. This pins the mechanism the `coverage`
    /// recipe and docs rely on.
    #[rudzio::test]
    async fn show_env_sh_is_export_prefixed_bare_is_not(
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
        write_token_fixture(ctx.consumer_workspace(), &rudzio_git_url(), &rev)?;
        let workspace: &Path = ctx.consumer_workspace();
        let cargo_home = cargo_home_with_full_graph();

        let sh = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir(), "--sh")?;
        if !sh.status.success() {
            anyhow::bail!(
                "`cargo llvm-cov show-env --sh` failed (is the `llvm-tools` rustup component \
                 missing?): exited {:?}.\n--- stdout ---\n{}\n--- stderr ---\n{}",
                sh.status.code(),
                String::from_utf8_lossy(&sh.stdout),
                String::from_utf8_lossy(&sh.stderr),
            );
        }
        let sh_text = String::from_utf8_lossy(&sh.stdout);
        anyhow::ensure!(
            sh_text.contains("export "),
            "`cargo llvm-cov show-env --sh` did not emit `export `-prefixed lines; the \
             `coverage` recipe's `eval` would not propagate variables:\n{sh_text}",
        );

        let bare = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir(), "")?;
        if !bare.status.success() {
            anyhow::bail!(
                "`cargo llvm-cov show-env` (bare) failed: exited {:?}.\n--- stderr ---\n{}",
                bare.status.code(),
                String::from_utf8_lossy(&bare.stderr),
            );
        }
        let bare_text = String::from_utf8_lossy(&bare.stdout);
        anyhow::ensure!(
            !bare_text.contains("export "),
            "bare `cargo llvm-cov show-env` unexpectedly emitted `export `-prefixed output; \
             the non-exporting footgun documented for this spec is gone:\n{bare_text}",
        );

        Ok(())
    }

    /// **Migration Bug #3 (operator env)** — under `show-env --sh` (the
    /// only `eval`-propagating form), `cargo rudzio test -p token` must
    /// write at least one new `.profraw` and `cargo llvm-cov report` must
    /// attribute non-zero line coverage to `src/asm/backend.rs`.
    #[rudzio::test]
    async fn cargo_rudzio_test_emits_profraw_for_operator_env(
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
        write_token_fixture(ctx.consumer_workspace(), &rudzio_git_url(), &rev)?;

        let workspace: &Path = ctx.consumer_workspace();
        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        let show_env = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir(), "--sh")?;
        if !show_env.status.success() {
            anyhow::bail!(
                "`cargo llvm-cov show-env --sh` failed (is the `llvm-tools` rustup component \
                 missing?): exited {:?}. Enter `nix develop`, which provides cargo-llvm-cov \
                 and the llvm-tools rustup component.\n--- stdout ---\n{}\n--- stderr ---\n{}",
                show_env.status.code(),
                String::from_utf8_lossy(&show_env.stdout),
                String::from_utf8_lossy(&show_env.stderr),
            );
        }
        let coverage_env = parse_show_env(&show_env.stdout)?;

        // Baseline: no pre-existing `.profraw` in the per-test target dir.
        let baseline_profraw = count_profraw(ctx.cargo_target_dir())?;
        anyhow::ensure!(
            baseline_profraw == 0,
            "per-test target dir had {baseline_profraw} pre-existing `.profraw` before the run; \
             expected a clean dir"
        );

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

        let profraw_count = count_profraw(ctx.cargo_target_dir())?;
        anyhow::ensure!(
            profraw_count > baseline_profraw,
            "`cargo rudzio test` under coverage produced ZERO new *.profraw files in {}; \
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

    /// **Migration Bug #3 (partial coverage env)** — when `CARGO_LLVM_COV`
    /// is present but `RUSTC_WRAPPER` (or `LLVM_PROFILE_FILE`) is missing —
    /// the exact state a bare `eval "$(cargo llvm-cov show-env)"` leaves a
    /// child process in — `cargo rudzio test` must error loudly instead of
    /// silently building an uninstrumented runner and writing zero
    /// `.profraw`.
    #[rudzio::test]
    async fn cargo_rudzio_errors_loudly_on_partial_coverage_env(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let rev = current_git_head()
            .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))?;
        write_token_fixture(ctx.consumer_workspace(), &rudzio_git_url(), &rev)?;

        let workspace: &Path = ctx.consumer_workspace();
        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        // A coverage environment where the marker and target dir are set but
        // the wrapper and profile-file variables are missing (what a bare
        // `eval "$(cargo llvm-cov show-env)"` leaves in a child).
        let partial_env: Vec<(String, String)> = vec![
            ("CARGO_LLVM_COV".to_owned(), "1".to_owned()),
            (
                "CARGO_LLVM_COV_TARGET_DIR".to_owned(),
                ctx.cargo_target_dir().to_string_lossy().into_owned(),
            ),
            (
                "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES".to_owned(),
                "token,token_tests".to_owned(),
            ),
        ];

        let test_output = run_cargo_rudzio_test(
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            workspace,
            &partial_env,
        )?;
        anyhow::ensure!(
            !test_output.status.success(),
            "`cargo rudzio test` with a partial coverage env (marker set, RUSTC_WRAPPER and \
             LLVM_PROFILE_FILE missing) exited successfully; it must error loudly instead of \
             silently building an uninstrumented runner.\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );

        let stderr = String::from_utf8_lossy(&test_output.stderr);
        anyhow::ensure!(
            stderr.contains("RUSTC_WRAPPER") && stderr.contains("LLVM_PROFILE_FILE"),
            "the loud error must name the missing coverage variables (RUSTC_WRAPPER and \
             LLVM_PROFILE_FILE); got:\n{stderr}",
        );

        Ok(())
    }
}
