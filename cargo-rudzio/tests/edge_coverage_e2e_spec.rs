//! Proactive edge-case e2e coverage for `cargo rudzio test` under
//! `cargo llvm-cov show-env --sh` instrumentation.
//!
//! The three migration Bug #3 specs
//! ([`super::ground_truth_bug_03_coverage_e2e_spec`],
//! [`super::migration_bug_03_coverage_profraw_spec`],
//! [`super::migration_bug_03_token_exact_e2e_spec`],
//! [`super::migration_bug_03_token_operator_env_e2e_spec`]) pin the *happy
//! path* and the *partial-env* failure for the operator's single-lib `token`
//! crate. This spec widens the surface to the shapes those specs do not
//! cover: multi-member workspaces, bin-only members whose `tests/cli.rs`
//! spawns the instrumented bin, `tests/` integration-test proxies, a
//! member `build.rs`, a self-named crate (Bug #7 `__rudzio_self_alias`
//! under coverage), `-p` package filtering, a two-run sequence with and
//! without `cargo llvm-cov clean --workspace`, the other half-set env
//! (`LLVM_PROFILE_FILE` without `CARGO_LLVM_COV`), the `--lcov` and
//! `--summary-only` report flows, and the Justfile `coverage` recipe's
//! reliance on `show-env --sh`.
//!
//! # Assertion strategy (shared)
//!
//! Every coverage case drives the operator's documented three-step flow
//! (`eval "$(cargo llvm-cov show-env --sh)"`; `cargo rudzio test`;
//! `cargo llvm-cov report`) and asserts, against the report JSON:
//!
//! 1. at least one NEW `*.profraw` appeared in the coverage target dir
//!    (instrumentation + profile-write half);
//! 2. `cargo llvm-cov report --json` exits 0;
//! 3. each member's REAL `src/` file is listed with non-zero line
//!    coverage (attribution half — the remap-prefix fix); and
//! 4. no generated bridge/aggregator file leaks into the report.
//!
//! # Fixture shape (operator-shaped)
//!
//! Workspaces root at `crates/*` with resolver `2` and a `file://` git
//! dependency on this checkout's `rudzio` (`runtime-futures` + `common`),
//! `warnings = "deny"`, `panic = "abort"` profiles, and a
//! `.cargo/config.toml` `build-dir` (so `CARGO_LLVM_COV_BUILD_DIR` stays
//! task-local instead of leaking the host's `~/.cargo/config.toml`).
//!
//! # Fresh build (no stale binary)
//!
//! Drives `cargo run -p cargo-rudzio -- test` (not
//! `env!("CARGO_BIN_EXE_cargo-rudzio")`) so the binary under test is a
//! fresh build into a task-local `CARGO_TARGET_DIR` /
//! `CARGO_BUILD_BUILD_DIR`.
//!
//! # Missing toolchain fails loudly
//!
//! `cargo llvm-cov` and its `llvm-tools` component must be present; the
//! toolchain probe fails naming the missing tool. No silent skip.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::SystemTime;

use anyhow::Context as _;
use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread;

/// Absolute `file://` git URL for the rudzio repo this test lives in.
fn rudzio_git_url() -> String {
    format!("file://{}", repo_root().display())
}

/// Resolve the ambient cargo home holding cargo-rudzio's full dep graph
/// (used for the `cargo run -p cargo-rudzio` fresh build).
fn cargo_home_with_full_graph() -> PathBuf {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// Resolve the toolchain pinned by the repo's `rust-toolchain.toml`.
///
/// `cargo llvm-cov` resolves its backing `llvm-profdata` from the toolchain
/// cargo selects for the subprocess's cwd. The consumer fixture lives in a
/// tempdir with no `rust-toolchain.toml`, so a bare `cargo llvm-cov report`
/// there falls back to the rustup default toolchain — which, on a host with
/// multiple toolchains, can have a DIFFERENT LLVM than the repo-pinned one
/// used to instrument the build, producing a raw-profile version mismatch.
/// Pinning `RUSTUP_TOOLCHAIN` to the repo's channel keeps the instrumenting
/// compiler and the merge step on the same LLVM.
fn repo_toolchain() -> Option<String> {
    let text = fs::read_to_string(repo_root().join("rust-toolchain.toml")).ok()?;
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("channel")?;
        let rest = rest.trim_start().strip_prefix('=')?;
        let value = rest.trim();
        value
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .map(str::to_owned)
    })
}

/// `RUSTUP_TOOLCHAIN` env entries pinning the repo toolchain, so every
/// spawned `cargo` (instrument + merge + clean) resolves the same LLVM.
fn toolchain_envs() -> impl Iterator<Item = (String, String)> {
    repo_toolchain()
        .into_iter()
        .map(|tc| ("RUSTUP_TOOLCHAIN".to_owned(), tc))
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

/// Run `cargo llvm-cov show-env --sh` from `workspace`.
fn llvm_cov_show_env(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
) -> io::Result<Output> {
    Command::new("cargo")
        .args(["llvm-cov", "show-env", "--sh"])
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .envs(toolchain_envs())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Parse `cargo llvm-cov show-env --sh` stdout into `(name, value)` pairs.
/// Each line is `export NAME='value'` (single-quoted only when shell
/// metacharacters are present); the raw bytes are preserved so the
/// unit-separator argument delimiter inside `__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS`
/// survives the round-trip.
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

/// Drive `cargo run -p cargo-rudzio -- test` from the repo root against
/// `workspace` with the coverage env applied, an optional `-p` package
/// filter, and any extra `test` args.
fn run_cargo_rudzio_test(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    workspace: &Path,
    package: Option<&str>,
    extra: &[&str],
    coverage_env: &[(String, String)],
) -> io::Result<Output> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut args: Vec<std::ffi::OsString> = vec![
        "run".into(),
        "-p".into(),
        "cargo-rudzio".into(),
        "--".into(),
        "test".into(),
        "--manifest-path".into(),
        workspace.join("Cargo.toml").into_os_string(),
    ];
    if let Some(pkg) = package {
        args.push("-p".into());
        args.push(pkg.into());
    }
    args.extend(extra.iter().map(std::ffi::OsString::from));
    Command::new(cargo_bin)
        .args(args)
        .current_dir(repo_root())
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .envs(toolchain_envs())
        .envs(
            coverage_env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Run `cargo llvm-cov report` with extra args against `workspace`.
fn run_llvm_cov_report(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    coverage_env: &[(String, String)],
    extra: &[&str],
) -> io::Result<Output> {
    let mut args: Vec<&str> = vec!["llvm-cov", "report"];
    args.extend(extra.iter().copied());
    Command::new("cargo")
        .args(args)
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .envs(toolchain_envs())
        .envs(
            coverage_env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Count `*.profraw` files directly under `cargo_target_dir` whose mtime is
/// at-or-after `since` (the run start), i.e. NEW profiles from this run.
fn count_new_profraw(cargo_target_dir: &Path, since: SystemTime) -> io::Result<usize> {
    let mut count = 0_usize;
    for entry_result in fs::read_dir(cargo_target_dir)? {
        let entry = entry_result?;
        if !entry.file_name().to_string_lossy().ends_with(".profraw") {
            continue;
        }
        if entry.metadata()?.modified()? >= since {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// Parse `cargo llvm-cov report --json` into `(filename, lines_covered)`.
fn parse_report_coverage(report_json: &str) -> anyhow::Result<Vec<(String, u64)>> {
    let report: serde_json::Value = serde_json::from_str(report_json)
        .map_err(|err| anyhow::anyhow!("failed to parse `cargo llvm-cov report --json`: {err}"))?;
    let files = report
        .pointer("/data/0/files")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("unexpected `--json` shape (no /data/0/files array): {report_json}")
        })?;
    files
        .iter()
        .map(|file| {
            let filename = file
                .get("filename")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("report file entry missing filename: {file}"))?;
            let covered = file
                .pointer("/summary/lines/covered")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow::anyhow!("report file entry missing /summary/lines/covered: {file}")
                })?;
            Ok((filename.to_owned(), covered))
        })
        .collect()
}

/// Assert `suffix` is listed with non-zero line coverage.
fn assert_src_covered(coverage: &[(String, u64)], suffix: &str) -> anyhow::Result<()> {
    let names: Vec<&str> = coverage.iter().map(|(name, _)| name.as_str()).collect();
    let covered = coverage
        .iter()
        .find(|(name, _)| name.ends_with(suffix))
        .ok_or_else(|| {
            anyhow::anyhow!("report did not list file ending {suffix:?}; listed: {names:?}")
        })?
        .1;
    anyhow::ensure!(
        covered > 0,
        "file ending {suffix:?} reported 0 covered lines"
    );
    Ok(())
}

/// Assert `suffix` is absent from the report.
fn assert_src_absent(coverage: &[(String, u64)], suffix: &str) -> anyhow::Result<()> {
    let leaked: Vec<&str> = coverage
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| name.ends_with(suffix))
        .collect();
    anyhow::ensure!(
        leaked.is_empty(),
        "report unexpectedly lists file ending {suffix:?}: {leaked:?}"
    );
    Ok(())
}

/// Assert no generated bridge/aggregator file leaked into the report.
fn assert_no_generated_files(coverage: &[(String, u64)]) -> anyhow::Result<()> {
    for (name, _) in coverage {
        anyhow::ensure!(
            !name.contains("rudzio-auto-runner")
                && !name.contains("_rudzio_bridge")
                && !name.contains("__rudzio_"),
            "generated bridge/aggregator file leaked into the report: {name}",
        );
    }
    Ok(())
}

/// Run the shared three-step coverage flow and return the parsed coverage.
fn run_coverage_flow(
    workspace: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    package: Option<&str>,
) -> anyhow::Result<Vec<(String, u64)>> {
    let show_env = llvm_cov_show_env(workspace, cargo_home, cargo_target_dir)?;
    anyhow::ensure!(
        show_env.status.success(),
        "`cargo llvm-cov show-env --sh` failed (llvm-tools component missing?): exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        show_env.status.code(),
        String::from_utf8_lossy(&show_env.stdout),
        String::from_utf8_lossy(&show_env.stderr),
    );
    let coverage_env = parse_show_env(&show_env.stdout)?;

    let run_start = SystemTime::now();
    let test_output = run_cargo_rudzio_test(
        cargo_home,
        cargo_target_dir,
        cargo_build_dir,
        workspace,
        package,
        &[],
        &coverage_env,
    )?;
    anyhow::ensure!(
        test_output.status.success(),
        "`cargo rudzio test` under coverage exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        test_output.status.code(),
        String::from_utf8_lossy(&test_output.stdout),
        String::from_utf8_lossy(&test_output.stderr),
    );
    let new_profraw = count_new_profraw(cargo_target_dir, run_start)?;
    anyhow::ensure!(
        new_profraw > 0,
        "`cargo rudzio test` under coverage produced zero NEW *.profraw in {}",
        cargo_target_dir.display(),
    );

    let report = run_llvm_cov_report(
        workspace,
        cargo_home,
        cargo_target_dir,
        &coverage_env,
        &["--json"],
    )?;
    anyhow::ensure!(
        report.status.success(),
        "`cargo llvm-cov report --json` exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        report.status.code(),
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr),
    );
    parse_report_coverage(&String::from_utf8_lossy(&report.stdout))
}

/// Write the operator-shaped workspace root manifest + `.cargo/config.toml`.
fn write_workspace_root(root: &Path, git_url: &str, rev: &str) -> anyhow::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
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
             rudzio = {{ git = \"{git_url}\", rev = \"{rev}\", features = [\"runtime-futures\", \"common\"] }}\n\
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
    .with_context(|| format!("writing workspace manifest at {}", root.display()))?;
    let cfg = root.join(".cargo").join("config.toml");
    fs::create_dir_all(cfg.parent().expect(".cargo has a parent"))?;
    fs::write(&cfg, "[build]\nbuild-dir = \"tmp/build-dir\"\n")?;
    Ok(())
}

/// `[package]` header shared by every member manifest.
fn package_header(name: &str) -> String {
    format!(
        "[package]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         publish = false\n\
         autotests = false\n"
    )
}

/// `[dev-dependencies]` footer shared by every member manifest.
const DEV_DEPS: &str = "[dev-dependencies]\n\
                        rudzio = { workspace = true }\n\
                        anyhow = { workspace = true }\n";

/// Write a lib-only member manifest (with optional extra sections).
fn write_lib_member_manifest(member: &Path, name: &str, extra: &str) -> anyhow::Result<()> {
    fs::write(
        member.join("Cargo.toml"),
        format!(
            "{package}\n\
             [lib]\n\
             name = \"{name}\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             {extra}{dev}",
            package = package_header(name),
            extra = extra,
            dev = DEV_DEPS,
        ),
    )
    .with_context(|| format!("writing lib member manifest at {}", member.display()))
}

/// Write a bin-only member manifest (`[[bin]]` + explicit `[[test]]`).
fn write_bin_member_manifest(
    member: &Path,
    name: &str,
    tests: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut manifest = format!(
        "{package}\n\
         [[bin]]\n\
         name = \"{name}\"\n\
         path = \"src/main.rs\"\n",
        package = package_header(name),
    );
    for (test_name, test_path) in tests {
        manifest.push_str(&format!(
            "\n[[test]]\n\
             name = \"{test_name}\"\n\
             path = \"{test_path}\"\n\
             harness = false\n"
        ));
    }
    manifest.push_str(&format!("\n{DEV_DEPS}"));
    fs::write(member.join("Cargo.toml"), manifest)
        .with_context(|| format!("writing bin member manifest at {}", member.display()))
}

/// Render a lib-only member's `src/lib.rs` with one inline `#[rudzio::test]`
/// calling `<name>_answer()`.
fn lib_source(name: &str) -> String {
    format!(
        "// rudzio_test marker\n\
         pub fn {name}_answer() -> u32 {{ 42 }}\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         #[rudzio::suite([\n\
         \x20\x20\x20\x20(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test),\n\
         ])]\n\
         mod tests {{\n\
         \x20\x20\x20\x20use super::*;\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn {name}_test(_t: &rudzio::common::context::Test) -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!({name}_answer() == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}}\n\
         }}\n"
    )
}

/// Write a lib member (`crates/<name>` with an inline suite).
fn write_lib_member(root: &Path, name: &str) -> anyhow::Result<()> {
    let member = root.join("crates").join(name);
    fs::create_dir_all(member.join("src"))?;
    write_lib_member_manifest(&member, name, "")?;
    fs::write(member.join("src").join("lib.rs"), lib_source(name))?;
    Ok(())
}

/// The real bin `src/main.rs` for a bin-only member.
fn bin_main_source() -> String {
    "fn compute() -> u32 { 6 * 7 }\n\nfn main() { println!(\"answer={}\", compute()); }\n"
        .to_owned()
}

/// A rudzio integration-test source that spawns the member's bin and asserts
/// its stdout contains the expected marker.
fn bin_cli_test_source(bin_name: &str, test_name: &str) -> String {
    format!(
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         use std::process::Command;\n\
         use rudzio::common::context::{{Suite, Test}};\n\
         use rudzio::runtime::futures::ThreadPool;\n\
         \n\
         #[rudzio::suite([(runtime = ThreadPool::new, suite = Suite, test = Test)])]\n\
         mod tests {{\n\
         \x20\x20\x20\x20use super::*;\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn {test_name}(_t: &Test) -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let out = Command::new(env!(\"CARGO_BIN_EXE_{bin_name}\")).output()?;\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(String::from_utf8_lossy(&out.stdout).contains(\"answer=42\"));\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}}\n\
         }}\n\
         \n\
         #[rudzio::main]\n\
         fn main() {{}}\n"
    )
}

/// Write a lib member's `tests/integration.rs` (a proxy that references the
/// member's real `src/` through the bridge crate alias).
fn integration_test_source(name: &str) -> String {
    format!(
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         use rudzio::common::context::{{Suite, Test}};\n\
         use rudzio::runtime::futures::ThreadPool;\n\
         \n\
         #[rudzio::suite([(runtime = ThreadPool::new, suite = Suite, test = Test)])]\n\
         mod tests {{\n\
         \x20\x20\x20\x20use super::*;\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn {name}_integration(_t: &Test) -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!({name}::{name}_answer() == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}}\n\
         }}\n\
         \n\
         #[rudzio::main]\n\
         fn main() {{}}\n"
    )
}

#[rudzio::suite([(runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use std::fs;
    use std::process::Command;

    use anyhow::Context as _;

    use super::{
        IsolatedCargoTest, Path, assert_no_generated_files, assert_src_absent, assert_src_covered,
        bin_cli_test_source, bin_main_source, cargo_home_with_full_graph, current_git_head,
        integration_test_source, lib_source, llvm_cov_show_env, llvm_cov_version_probe,
        parse_show_env, repo_root, rudzio_git_url, run_cargo_rudzio_test, run_coverage_flow,
        run_llvm_cov_report, toolchain_envs, write_bin_member_manifest, write_lib_member,
        write_lib_member_manifest, write_workspace_root,
    };

    fn probe_toolchain() -> anyhow::Result<()> {
        let version = llvm_cov_version_probe()?;
        anyhow::ensure!(
            version.status.success(),
            "`cargo llvm-cov` is not installed: `cargo llvm-cov --version` failed {:?}.\n--- stderr ---\n{}",
            version.status.code(),
            String::from_utf8_lossy(&version.stderr),
        );
        Ok(())
    }

    fn git_dep_fixture() -> anyhow::Result<(String, String)> {
        let rev = current_git_head()
            .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))?;
        let git_url = rudzio_git_url();
        Ok((git_url, rev))
    }

    /// **Case 1** — two lib members, workspace-wide: both members' real
    /// `src/` files are listed with non-zero coverage and no generated
    /// file leaks.
    #[rudzio::test]
    async fn two_lib_members_workspace_wide_coverage(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        // Replace the pre-warmed consumer template with the operator shape.
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;
        write_lib_member(workspace, "alpha")?;
        write_lib_member(workspace, "beta")?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            None,
        )?;
        assert_no_generated_files(&coverage)?;
        assert_src_covered(&coverage, "crates/alpha/src/lib.rs")?;
        assert_src_covered(&coverage, "crates/beta/src/lib.rs")?;
        Ok(())
    }

    /// **Case 2** — bin-only member whose `tests/cli.rs` spawns the
    /// instrumented bin via `env!("CARGO_BIN_EXE_<bin>")`: the runner must
    /// NOT clobber the member's real bin. The coverage runner-exposure path
    /// copies the runner next to itself under the first member's package
    /// name; when that name equals the member's own bin name the copy
    /// overwrites the real bin, so the CLI test spawns the runner (which
    /// re-runs the suite) instead of the bin. Asserted via `--no-run` (build
    /// only, no recursion) by proving the bin binary differs from the runner.
    #[rudzio::test]
    async fn bin_only_member_cli_spawn_does_not_clobber_bin(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;

        let modera = workspace.join("crates").join("modera");
        fs::create_dir_all(modera.join("src"))?;
        fs::create_dir_all(modera.join("tests"))?;
        write_bin_member_manifest(&modera, "modera", &[("cli", "tests/cli.rs")])?;
        fs::write(modera.join("src").join("main.rs"), bin_main_source())?;
        fs::write(
            modera.join("tests").join("cli.rs"),
            bin_cli_test_source("modera", "cli_spawns_bin"),
        )?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let show_env = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir())?;
        anyhow::ensure!(
            show_env.status.success(),
            "`cargo llvm-cov show-env --sh` failed (llvm-tools component missing?): exit {:?}\n--- stderr ---\n{}",
            show_env.status.code(),
            String::from_utf8_lossy(&show_env.stderr),
        );
        let coverage_env = parse_show_env(&show_env.stdout)?;
        let test_output = run_cargo_rudzio_test(
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            workspace,
            None,
            &["--no-run"],
            &coverage_env,
        )?;
        anyhow::ensure!(
            test_output.status.success(),
            "`cargo rudzio test --no-run` under coverage exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            test_output.status.code(),
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );

        let debug = ctx.cargo_target_dir().join("debug");
        let runner = debug.join("rudzio-auto-runner");
        let bin = debug.join("modera");
        anyhow::ensure!(
            runner.is_file(),
            "runner binary missing at {}",
            runner.display()
        );
        anyhow::ensure!(bin.is_file(), "member bin missing at {}", bin.display());
        let runner_bytes = fs::read(&runner)?;
        let bin_bytes = fs::read(&bin)?;
        anyhow::ensure!(
            runner_bytes != bin_bytes,
            "the member bin `{}` is byte-identical to the runner `{}`: the coverage runner-exposure \
             step clobbered the real bin, so a `tests/cli.rs` that spawns `CARGO_BIN_EXE_modera` \
             would run the runner (re-running the suite recursively) instead of the bin.",
            bin.display(),
            runner.display(),
        );
        Ok(())
    }

    /// **Case 3** — a lib member with a `tests/` integration-test proxy:
    /// both the member `src/` and the integration proxy are attributed to
    /// the member's real `src/` (no transient copy leaks).
    #[rudzio::test]
    async fn integration_test_proxy_attributed_to_member_src(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;

        let member = workspace.join("crates").join("gamma");
        fs::create_dir_all(member.join("src"))?;
        fs::create_dir_all(member.join("tests"))?;
        write_lib_member_manifest(
            &member,
            "gamma",
            "[[test]]\nname = \"integration\"\npath = \"tests/integration.rs\"\nharness = false\n",
        )?;
        fs::write(member.join("src").join("lib.rs"), lib_source("gamma"))?;
        fs::write(
            member.join("tests").join("integration.rs"),
            integration_test_source("gamma"),
        )?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            None,
        )?;
        assert_no_generated_files(&coverage)?;
        assert_src_covered(&coverage, "crates/gamma/src/lib.rs")?;
        Ok(())
    }

    /// **Case 4** — a lib member with a `build.rs` (build-script effects
    /// must still reach the member under coverage).
    #[rudzio::test]
    async fn build_script_member_coverage(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;

        let member = workspace.join("crates").join("delta");
        fs::create_dir_all(member.join("src"))?;
        write_lib_member_manifest(&member, "delta", "")?;
        fs::write(
            member.join("src").join("lib.rs"),
            "// rudzio_test marker\n\
             pub fn delta_answer() -> u32 { 42 }\n\
             \n\
             #[cfg(any(test, rudzio_test))]\n\
             #[rudzio::suite([\n\
             \x20\x20\x20\x20(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test),\n\
             ])]\n\
             mod tests {\n\
             \x20\x20\x20\x20use super::*;\n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn delta_test(_t: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(option_env!(\"COVERAGE_BUILD\") == Some(\"yes\"));\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(delta_answer() == 42);\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}\n\
             }\n",
        )?;
        fs::write(
            member.join("build.rs"),
            "fn main() {\n    println!(\"cargo:rustc-env=COVERAGE_BUILD=yes\");\n}\n",
        )?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            None,
        )?;
        assert_no_generated_files(&coverage)?;
        assert_src_covered(&coverage, "crates/delta/src/lib.rs")?;
        Ok(())
    }

    /// **Case 5** — a self-named crate (Bug #7 `__rudzio_self_alias` path):
    /// `pub mod project;` inside crate `project`, exercised under coverage.
    #[rudzio::test]
    async fn self_named_module_member_coverage(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;

        let member = workspace.join("crates").join("project");
        fs::create_dir_all(member.join("src"))?;
        write_lib_member_manifest(
            &member,
            "project",
            "[[test]]\nname = \"integration\"\npath = \"tests/integration/mod.rs\"\nharness = false\n",
        )?;
        fs::write(member.join("src").join("lib.rs"), "pub mod project;\n")?;
        fs::write(
            member.join("src").join("project.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        fs::create_dir_all(member.join("tests").join("integration"))?;
        fs::write(
            member.join("tests").join("integration").join("mod.rs"),
            "// rudzio_test marker\n\
             #![allow(clippy::tests_outside_test_module)]\n\
             \n\
             use rudzio::common::context::{Suite, Test};\n\
             use rudzio::runtime::futures::ThreadPool;\n\
             \n\
             #[rudzio::suite([(runtime = ThreadPool::new, suite = Suite, test = Test)])]\n\
             mod tests {\n\
             \x20\x20\x20\x20use project::project::answer;\n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn self_named_answer() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42);\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}\n\
             }\n\
             \n\
             #[rudzio::main]\n\
             fn main() {}\n",
        )?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            None,
        )?;
        assert_no_generated_files(&coverage)?;
        assert_src_covered(&coverage, "crates/project/src/project.rs")?;
        Ok(())
    }

    /// **Case 6** — `-p` filtering: running `-p alpha` attributes coverage
    /// to `alpha` only; `beta`'s `src/` is absent.
    #[rudzio::test]
    async fn package_filter_only_selected_member_files(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;
        write_lib_member(workspace, "alpha")?;
        write_lib_member(workspace, "beta")?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            Some("alpha"),
        )?;
        assert_no_generated_files(&coverage)?;
        assert_src_covered(&coverage, "crates/alpha/src/lib.rs")?;
        assert_src_absent(&coverage, "crates/beta/src/lib.rs")?;
        Ok(())
    }

    /// **Case 7a** — a two-run sequence with `cargo llvm-cov clean
    /// --workspace` between runs. `clean` delegates to `cargo clean`, which
    /// removes the `debug/`/`release/` subdirs but NOT the `.profraw` files
    /// that `cargo llvm-cov 0.8.x` (where `CARGO_LLVM_COV_TARGET_DIR ==
    /// CARGO_TARGET_DIR`) dumps at the target-dir root. The prior member's
    /// stale `.profraw` therefore still leaks into the second report — the
    /// same merge-everything behaviour as stock `cargo llvm-cov`.
    #[rudzio::test]
    async fn stale_profraw_leaks_despite_clean_workspace(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;
        write_lib_member(workspace, "alpha")?;
        write_lib_member(workspace, "beta")?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");

        // Run 1: cover `alpha`.
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            Some("alpha"),
        )?;
        assert_src_covered(&coverage, "crates/alpha/src/lib.rs")?;

        // Clean removes the prior profraws.
        let show_env = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir())?;
        anyhow::ensure!(
            show_env.status.success(),
            "`cargo llvm-cov show-env --sh` failed: exit {:?}",
            show_env.status.code(),
        );
        let coverage_env = parse_show_env(&show_env.stdout)?;
        // `cargo llvm-cov clean --workspace` is a top-level subcommand.
        let clean_out = Command::new("cargo")
            .args(["llvm-cov", "clean", "--workspace"])
            .current_dir(workspace)
            .env("CARGO_HOME", &cargo_home)
            .env("CARGO_TARGET_DIR", ctx.cargo_target_dir())
            .env("CARGO_NET_OFFLINE", "true")
            .env("NO_COLOR", "1")
            .envs(toolchain_envs())
            .envs(
                coverage_env
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .output()?;
        anyhow::ensure!(
            clean_out.status.success(),
            "`cargo llvm-cov clean --workspace` exited {:?}\n--- stderr ---\n{}",
            clean_out.status.code(),
            String::from_utf8_lossy(&clean_out.stderr),
        );

        // Run 2: cover `beta`; alpha's stale profraw must be gone.
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            Some("beta"),
        )?;
        assert_src_covered(&coverage, "crates/beta/src/lib.rs")?;
        // `clean --workspace` did not remove run 1's root-level `.profraw`,
        // so alpha's stale profile leaks into run 2's report.
        assert_src_covered(&coverage, "crates/alpha/src/lib.rs")?;
        Ok(())
    }

    /// **Case 7b** — two runs WITHOUT clean: the prior member's stale
    /// `.profraw` leaks into the report (documenting parity with stock
    /// `cargo llvm-cov`, which merges every `.profraw` in the target dir).
    #[rudzio::test]
    async fn stale_profraw_leaks_without_clean(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;
        write_lib_member(workspace, "alpha")?;
        write_lib_member(workspace, "beta")?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");

        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            Some("alpha"),
        )?;
        assert_src_covered(&coverage, "crates/alpha/src/lib.rs")?;

        // No clean between runs.
        let coverage = run_coverage_flow(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            Some("beta"),
        )?;
        assert_src_covered(&coverage, "crates/beta/src/lib.rs")?;
        // Stale `alpha` profraw from run 1 leaks into run 2's report.
        assert_src_covered(&coverage, "crates/alpha/src/lib.rs")?;
        Ok(())
    }

    /// **Case 8a** — `CARGO_LLVM_COV=1` with `RUSTC_WRAPPER` missing must
    /// error loudly, naming `RUSTC_WRAPPER` and the `--sh` fix, with the
    /// exact message body from `coverage::validate_wrapper_env`.
    #[rudzio::test]
    async fn half_set_env_missing_rustc_wrapper_errors_loudly(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;
        write_lib_member(workspace, "alpha")?;

        let cargo_home = cargo_home_with_full_graph();
        let partial_env: Vec<(String, String)> = vec![
            ("CARGO_LLVM_COV".to_owned(), "1".to_owned()),
            (
                "CARGO_LLVM_COV_TARGET_DIR".to_owned(),
                ctx.cargo_target_dir().to_string_lossy().into_owned(),
            ),
            (
                "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES".to_owned(),
                "alpha,alpha_tests".to_owned(),
            ),
        ];
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let test_output = run_cargo_rudzio_test(
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            workspace,
            None,
            &[],
            &partial_env,
        )?;
        anyhow::ensure!(
            !test_output.status.success(),
            "`cargo rudzio test` with a half-set coverage env (marker set, RUSTC_WRAPPER missing) \
             exited successfully; it must error loudly.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );
        let stderr = String::from_utf8_lossy(&test_output.stderr);
        anyhow::ensure!(
            stderr.contains("coverage environment is incomplete"),
            "the loud error must say the coverage environment is incomplete; got:\n{stderr}",
        );
        anyhow::ensure!(
            stderr.contains("show-env --sh"),
            "the loud error must point at the `show-env --sh` fix; got:\n{stderr}",
        );
        Ok(())
    }

    /// **Case 8b** — `LLVM_PROFILE_FILE` set WITHOUT `CARGO_LLVM_COV` is NOT
    /// coverage; `cargo rudzio test` runs normally (exit 0, tests pass) and
    /// produces no instrumented build.
    #[rudzio::test]
    async fn llvm_profile_file_without_cargo_llvm_cov_runs_normally(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;
        write_lib_member(workspace, "alpha")?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let only_profile: Vec<(String, String)> = vec![(
            "LLVM_PROFILE_FILE".to_owned(),
            ctx.cargo_target_dir()
                .join("orphan-%p.profraw")
                .to_string_lossy()
                .into_owned(),
        )];
        let test_output = run_cargo_rudzio_test(
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            workspace,
            None,
            &[],
            &only_profile,
        )?;
        anyhow::ensure!(
            test_output.status.success(),
            "`cargo rudzio test` with only `LLVM_PROFILE_FILE` (no `CARGO_LLVM_COV`) exited {:?}; \
             expected a normal, uninstrumented run.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            test_output.status.code(),
            String::from_utf8_lossy(&test_output.stdout),
            String::from_utf8_lossy(&test_output.stderr),
        );
        Ok(())
    }

    /// **Case 9** — after `cargo rudzio test`, both `report --lcov
    /// --output-path` and `report --summary-only` exit 0.
    #[rudzio::test]
    async fn lcov_and_summary_only_reports_work(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        probe_toolchain()?;
        let (git_url, rev) = git_dep_fixture()?;
        let workspace: &Path = ctx.consumer_workspace();
        let consumer = workspace.join("consumer");
        if consumer.exists() {
            fs::remove_dir_all(&consumer)?;
        }
        let old_manifest = workspace.join("Cargo.toml");
        if old_manifest.exists() {
            fs::remove_file(&old_manifest)?;
        }
        write_workspace_root(workspace, &git_url, &rev)?;
        write_lib_member(workspace, "alpha")?;

        let cargo_home = cargo_home_with_full_graph();
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .unwrap()
            .join("cargo-build-dir");
        let show_env = llvm_cov_show_env(workspace, &cargo_home, ctx.cargo_target_dir())?;
        anyhow::ensure!(
            show_env.status.success(),
            "`cargo llvm-cov show-env --sh` failed (llvm-tools component missing?): exit {:?}",
            show_env.status.code(),
        );
        let coverage_env = parse_show_env(&show_env.stdout)?;
        let test_output = run_cargo_rudzio_test(
            &cargo_home,
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            workspace,
            None,
            &[],
            &coverage_env,
        )?;
        anyhow::ensure!(
            test_output.status.success(),
            "`cargo rudzio test` under coverage exited {:?}\n--- stderr ---\n{}",
            test_output.status.code(),
            String::from_utf8_lossy(&test_output.stderr),
        );

        let lcov_path = ctx.cargo_target_dir().parent().unwrap().join("lcov.info");
        let lcov = run_llvm_cov_report(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &coverage_env,
            &["--lcov", "--output-path", lcov_path.to_str().unwrap()],
        )?;
        anyhow::ensure!(
            lcov.status.success(),
            "`cargo llvm-cov report --lcov --output-path` exited {:?}\n--- stderr ---\n{}",
            lcov.status.code(),
            String::from_utf8_lossy(&lcov.stderr),
        );
        anyhow::ensure!(
            lcov_path.is_file(),
            "`--lcov --output-path` did not write {}",
            lcov_path.display(),
        );

        let summary = run_llvm_cov_report(
            workspace,
            &cargo_home,
            ctx.cargo_target_dir(),
            &coverage_env,
            &["--summary-only"],
        )?;
        anyhow::ensure!(
            summary.status.success(),
            "`cargo llvm-cov report --summary-only` exited {:?}\n--- stderr ---\n{}",
            summary.status.code(),
            String::from_utf8_lossy(&summary.stderr),
        );
        Ok(())
    }

    /// **Case 10** — the Justfile `coverage` recipe relies on
    /// `show-env --sh` (the only `eval`-propagating form). Guard against a
    /// regression of the doc fix back to the non-exporting bare form.
    #[rudzio::test]
    async fn justfile_coverage_recipe_uses_show_env_sh(
        _ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let justfile = repo_root().join("Justfile");
        let text = fs::read_to_string(&justfile)
            .with_context(|| format!("reading Justfile at {}", justfile.display()))?;
        let recipe = text
            .split_once("coverage:")
            .map(|(_, rest)| {
                rest.split_once("\n\n")
                    .map(|(head, _)| head)
                    .unwrap_or(rest)
            })
            .ok_or_else(|| anyhow::anyhow!("Justfile has no `coverage:` recipe"))?;
        anyhow::ensure!(
            recipe.contains("show-env --sh"),
            "the Justfile `coverage` recipe must `eval \"$(cargo llvm-cov show-env --sh)\"` \
             (the bare form emits non-exported assignments); recipe:\n{recipe}",
        );
        Ok(())
    }
}
