//! Proactive edge-case e2e coverage for a sibling member that dev-depends
//! on another member carrying a `[lib] name` override (0.2.0 release gap).
//!
//! Cargo's extern-name rule: a non-renamed dependency's crate name is its
//! **lib name** (honouring `[lib] name`), not its package name. cargo-rudzio
//! re-emits a bridged member's sibling dev-deps into the bridge's
//! `[dependencies]`, so it must key a sibling entry by that sibling's lib
//! identifier. Keying by package name leaves the consumer's
//! `use <lib_name>::…` unresolved (E0433).

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, RunSummary, assert_exact_test_names, assert_fresh_binary,
    assert_summary, combined_output, current_git_head, repo_root,
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
#[rudzio::suite([(runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, RunSummary, assert_exact_test_names, assert_summary, cargo_build_dir_for,
        combined_output, fs, rudzio_git_rev, run_cargo_rudzio, write_lib_member_manifest,
        write_test_source, write_workspace_manifest,
    };

    /// Consumer `a` dev-depends on sibling `b` (`[lib] name = "renamed_lib"`)
    /// and its test references `b` by that lib name. The bridge must key the
    /// sibling dev-dep by `renamed_lib`, not `b`, or `use renamed_lib::answer;`
    /// fails with E0433.
    #[rudzio::test]
    async fn sibling_dev_dep_lib_name_override_resolves(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("sibling-lib-name-override");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;

        // Sibling `b`: lib name `renamed_lib`, exports `answer`, and carries
        // its own rudzio test so it is bridged (a prerequisite for the
        // sibling-bridge redirect under test).
        let dep_b = workspace.join("crates").join("b");
        fs::create_dir_all(dep_b.join("src"))?;
        fs::create_dir_all(dep_b.join("tests").join("integration"))?;
        write_lib_member_manifest(&dep_b, "b", "renamed_lib")?;
        fs::write(
            dep_b.join("src").join("lib.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        write_test_source(
            &dep_b.join("tests").join("integration"),
            "    use renamed_lib::answer;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20async fn b_own_answer() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42, \"expected answer() == 42\");\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}\n",
        )?;

        // Consumer `a`: dev-depends on `b` and its test imports `b`'s lib
        // name.
        let consumer_a = workspace.join("crates").join("a");
        fs::create_dir_all(consumer_a.join("src"))?;
        fs::create_dir_all(consumer_a.join("tests").join("integration"))?;
        fs::write(
            consumer_a.join("Cargo.toml"),
            "[package]\n\
             name = \"a\"\n\
             version = \"0.1.0\"\n\
             edition.workspace = true\n\
             publish = false\n\
             autotests = false\n\
             \n\
             [lib]\n\
             name = \"a\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [[test]]\n\
             name = \"integration\"\n\
             path = \"tests/integration/mod.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             b = { path = \"../b\" }\n\
             rudzio = { workspace = true }\n\
             anyhow = { workspace = true }\n\
             \n\
             [lints]\n\
             workspace = true\n",
        )?;
        fs::write(
            consumer_a.join("src").join("lib.rs"),
            "pub fn a_item() -> u32 {\n    1\n}\n",
        )?;
        write_test_source(
            &consumer_a.join("tests").join("integration"),
            "    use renamed_lib::answer;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20async fn a_uses_sibling_by_lib_name() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42, \"expected answer() == 42\");\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}\n",
        )?;

        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        let text = combined_output(&output);
        anyhow::ensure!(output.status.success(), "expected exit 0, output:\n{text}");
        assert_summary(&text, RunSummary::plain(2, 0, 0, 0, 0, 0, 0, 2))?;
        assert_exact_test_names(&text, &["a_uses_sibling_by_lib_name", "b_own_answer"])?;
        Ok(())
    }
}
