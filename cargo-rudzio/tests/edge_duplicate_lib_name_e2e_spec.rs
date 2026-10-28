//! Proactive edge-case e2e coverage for two workspace members that declare
//! the same `[lib] name` (0.2.0 release gap).
//!
//! Cargo permits distinct packages sharing a lib name in one workspace (it
//! warns about the `lib<name>.rlib` output collision), but a single crate
//! depending on both links only one of them. The aggregator depends on
//! every bridge, so cargo-rudzio must give colliding bridges distinct crate
//! names instead of silently dropping one `extern crate`.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, assert_fresh_binary, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread;

/// The `#[rudzio::suite]` preamble shared by every fixture test file.
const SUITE_HEADER: &str = "// rudzio_test marker\n\
#![allow(clippy::tests_outside_test_module)]\n\
\n\
use rudzio::common::context::{Suite, Test};\n\
use rudzio::runtime::futures::ThreadPool;\n\
\n\
#[rudzio::suite([(runtime = ThreadPool::new, suite = Suite, test = Test)])]\n\
mod tests {\n";

/// The `#[rudzio::main]` entry point every fixture test binary needs.
const SUITE_FOOTER: &str = "}\n\n#[rudzio::main]\nfn main() {}\n";

/// The checkout's real HEAD SHA, used as the `rev` pin for the git dep.
fn rudzio_git_rev() -> anyhow::Result<String> {
    current_git_head()
        .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))
}

/// The absolute `file://` git URL for the rudzio repo this test lives in.
fn rudzio_git_url() -> String {
    format!("file://{}", repo_root().display())
}

/// Write the operator-shaped workspace root manifest: `crates/*`, resolver
/// 2, `file://` git rudzio dep (runtime-futures/runtime-compio/common),
/// warnings deny, panic-abort profiles, and the `.cargo/config.toml` build-dir.
fn write_workspace_manifest(root: &Path, git_rev: &str) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let git_url = rudzio_git_url();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"crates/*\"]\n\
             resolver = \"2\"\n\
             \n\
             [workspace.dependencies]\n\
             anyhow = \"=1.0.104\"\n\
             rudzio = {{ git = \"{git_url}\", rev = \"{git_rev}\", features = [\"runtime-futures\", \"runtime-compio\", \"common\"] }}\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.lints.rust]\n\
             warnings = \"deny\"\n\
             unsafe_code = \"deny\"\n\
             unexpected_cfgs = {{ level = \"warn\", check-cfg = ['cfg(rudzio_test)'] }}\n\
             \n\
             [profile.dev]\n\
             panic = \"abort\"\n\
             \n\
             [profile.release]\n\
             panic = \"abort\"\n",
        ),
    )?;
    fs::create_dir_all(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo").join("config.toml"),
        "[build]\nbuild-dir = \"tmp/build-dir\"\n",
    )
}

/// Write a lib member manifest: package `package_name` with `[lib] name =
/// lib_name`, a single `tests/integration/mod.rs` harness-false `[[test]]`,
/// and a rudzio + anyhow dev-dep.
fn write_lib_member_manifest(
    member_dir: &Path,
    package_name: &str,
    lib_name: &str,
) -> io::Result<()> {
    fs::write(
        member_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"{package_name}\"\n\
             version = \"0.1.0\"\n\
             edition.workspace = true\n\
             publish = false\n\
             autotests = false\n\
             \n\
             [lib]\n\
             name = \"{lib_name}\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [[test]]\n\
             name = \"integration\"\n\
             path = \"tests/integration/mod.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             rudzio = {{ workspace = true }}\n\
             anyhow = {{ workspace = true }}\n\
             \n\
             [lints]\n\
             workspace = true\n",
        ),
    )
}

/// Write a fixture integration test source: `SUITE_HEADER` + `body` +
/// `SUITE_FOOTER`.
fn write_test_source(test_dir: &Path, body: &str) -> io::Result<()> {
    fs::create_dir_all(test_dir)?;
    fs::write(
        test_dir.join("mod.rs"),
        format!("{SUITE_HEADER}{body}{SUITE_FOOTER}"),
    )
}

/// Build a task-local cargo build dir beside the cargo target dir.
fn cargo_build_dir_for(cargo_target_dir: &Path) -> anyhow::Result<PathBuf> {
    Ok(cargo_target_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
        .join("cargo-build-dir"))
}

/// Drive the freshness-checked cargo-rudzio binary against `fixture_root`.
fn run_cargo_rudzio(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    fixture_root: &Path,
    args: &[&str],
) -> anyhow::Result<Output> {
    let binary = assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;
    let output = Command::new(binary)
        .arg("test")
        .args(args)
        .current_dir(fixture_root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(io::Error::other)?;
    Ok(output)
}

/// Assert exit 0 and that the runner reported exactly the `expected` test
/// set (every name present, summary count equals `expected.len()`).
fn assert_exact_passed(output: &Output, expected: &[&str]) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        output.status.success(),
        "cargo rudzio test must exit 0; exit = {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code(),
    );
    for name in expected {
        anyhow::ensure!(
            stdout.contains(name),
            "runner stdout must name the aggregated test `{name}`.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }
    anyhow::ensure!(
        stdout.contains(&format!("{} passed", expected.len())),
        "runner must report exactly {} passed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        expected.len(),
    );
    Ok(())
}

#[rudzio::suite([(runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, assert_exact_passed, cargo_build_dir_for, fs, rudzio_git_rev,
        run_cargo_rudzio, write_lib_member_manifest, write_test_source, write_workspace_manifest,
    };

    /// Packages `p1` and `p2` both declare `[lib] name = "shared"`; each has
    /// its own rudzio test importing `shared::<fn>`. Both tests must run.
    #[rudzio::test]
    async fn two_members_same_lib_name_both_run(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("duplicate-lib-name");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;

        for (package, func, value, test_name) in [
            ("p1", "one", 1_u32, "p1_shared_one"),
            ("p2", "two", 2_u32, "p2_shared_two"),
        ] {
            let member = workspace.join("crates").join(package);
            fs::create_dir_all(member.join("src"))?;
            fs::create_dir_all(member.join("tests").join("integration"))?;
            write_lib_member_manifest(&member, package, "shared")?;
            fs::write(
                member.join("src").join("lib.rs"),
                format!("pub fn {func}() -> u32 {{\n    {value}\n}}\n"),
            )?;
            write_test_source(
                &member.join("tests").join("integration"),
                &format!(
                    "    use shared::{func};\n\
                     \n\
                     \x20\x20\x20\x20#[rudzio::test]\n\
                     \x20\x20\x20\x20async fn {test_name}() -> anyhow::Result<()> {{\n\
                     \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!({func}() == {value}, \"expected {func}() == {value}\");\n\
                     \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
                     \x20\x20\x20\x20}}\n"
                ),
            )?;
        }

        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["p1_shared_one", "p2_shared_two"])
    }
}
