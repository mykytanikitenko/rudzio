//! End-to-end edge-case coverage for Bug #8: bin-only workspace members
//! (a `[[bin]]` with no `[lib]`) must aggregate their integration tests
//! into the runner via the stub bridge lib (`stub_bridge_lib_body`) and
//! the shared `emit_proxy_mounts` path.
//!
//! # Symptom (pre-fix)
//!
//! A bin-only member produced a stub `src/lib.rs` with zero test-proxy
//! mounts, so its integration tests never reached `TEST_TOKENS` and — when
//! it was the only member — the runner panicked at startup on
//! `linkme::from_raw_parts(NULL, 0)`.
//!
//! # Assertion strategy
//!
//! Each case materialises a synthetic workspace under a per-test tempdir,
//! runs `cargo rudzio test`, and asserts BOTH the exit status AND the exact
//! set of test names reported passed. The set is pinned by parsing the
//! runner's `test result: ok. N passed; ...` summary line and requiring
//! `N` to equal the expected count while every expected name appears in
//! stdout — a full set assertion, not a single-`contains` check.

use std::env;
use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/`. The fixture's `rudzio` workspace
/// dependency points here so the synthetic workspace builds against the
/// code under test rather than a published version.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Write the workspace root manifest listing `members`.
fn write_workspace_manifest(root: &Path, members: &[&str]) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    let member_list = members
        .iter()
        .map(|member| format!("\"{member}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [{member_list}]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )
}

/// Write a bin-only member's `Cargo.toml`: a `[[bin]]` (no `[lib]`) plus
/// the given explicit `[[test]]` entries, with `autotests = false`.
/// `tests` is a list of `(test_name, test_path_relative_to_member)`.
fn write_bin_only_manifest(
    member_dir: &Path,
    name: &str,
    tests: &[(&str, &str)],
) -> io::Result<()> {
    let mut manifest = format!(
        "[package]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [[bin]]\n\
         name = \"{name}\"\n\
         path = \"src/main.rs\"\n"
    );
    for (test_name, test_path) in tests {
        let _appended: fmt::Result = write!(
            manifest,
            "\n[[test]]\n\
             name = \"{test_name}\"\n\
             path = \"{test_path}\"\n\
             harness = false\n"
        );
    }
    manifest.push_str(
        "\n[dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    );
    fs::write(member_dir.join("Cargo.toml"), manifest)
}

/// A rudzio integration-test source with a single `#[rudzio::test]` named
/// `test_name` whose body asserts `body` before returning `Ok(())`.
fn integration_test_source(test_name: &str, body: &str) -> String {
    format!(
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         use rudzio::common::context::{{Suite, Test}};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {{\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn {test_name}() -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20{body}\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}}\n\
         }}\n\
         \n\
         #[rudzio::main]\n\
         fn main() {{}}\n",
    )
}

/// `src/main.rs` for a bin-only member whose default bin carries its own
/// `#[rudzio::test]` surface (the Bug #19 bin-source-test path).
fn bin_source_with_test_source(test_name: &str) -> String {
    format!(
        "// rudzio_test marker\n\
         fn main() {{\n\
         \x20\x20\x20\x20println!(\"stub\");\n\
         }}\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {{\n\
         \x20\x20\x20\x20use rudzio::common::context::{{Suite, Test}};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn {test_name}() -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(2 + 2 == 4, \"sanity\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}}\n\
         \x20\x20\x20\x20}}\n\
         }}\n"
    )
}

/// Spawn `cargo rudzio test` against `root` with the isolated cargo env.
fn run_cargo_rudzio(
    root: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    args: &[&str],
) -> io::Result<Output> {
    Command::new(
        cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .map_err(io::Error::other)?,
    )
    .arg("test")
    .args(args)
    .current_dir(root)
    .env(
        "CARGO",
        env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
    )
    .env("CARGO_HOME", cargo_home)
    .env("CARGO_TARGET_DIR", cargo_target_dir)
    .env("CARGO_NET_OFFLINE", "true")
    .env("NO_COLOR", "1")
    .output()
}

/// Extract the `N` from the runner's `test result: ok. N passed; ...` line.
fn parse_passed_count(stdout: &str) -> Option<usize> {
    let line = stdout
        .lines()
        .find(|candidate| candidate.contains("test result:") && candidate.contains("passed;"))?;
    let (head, _tail) = line.split_once(" passed;")?;
    head.rsplit(|ch: char| !ch.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Assert the run exited 0 AND reported exactly `expected` as the passed
/// test set: summary count equals `expected.len()` and every expected name
/// appears in stdout.
fn assert_exact_passed(output: &Output, expected: &[&str]) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        output.status.success(),
        "cargo rudzio test must exit 0; exit = {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code(),
    );
    let passed = parse_passed_count(&stdout).ok_or_else(|| {
        anyhow::anyhow!(
            "could not parse `N passed;` from the runner summary\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        )
    })?;
    anyhow::ensure!(
        passed == expected.len(),
        "runner summary reports {passed} passed, expected exactly {} ({expected:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        expected.len(),
    );
    for name in expected {
        anyhow::ensure!(
            stdout.contains(name),
            "test `{name}` must be reported in runner stdout\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }
    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, TempDir, assert_exact_passed, bin_source_with_test_source, fs,
        integration_test_source, run_cargo_rudzio, write_bin_only_manifest,
        write_workspace_manifest,
    };

    #[rudzio::test]
    fn bin_only_member_aggregates_single_integration_test(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["mybin"])?;

        let mybin = root.join("mybin");
        fs::create_dir_all(mybin.join("src"))?;
        fs::create_dir_all(mybin.join("tests"))?;
        write_bin_only_manifest(&mybin, "mybin", &[("integration", "tests/integration.rs")])?;
        fs::write(mybin.join("src").join("main.rs"), "fn main() {}\n")?;
        fs::write(
            mybin.join("tests").join("integration.rs"),
            integration_test_source("bin_only_single_test", "anyhow::ensure!(2 + 2 == 4);"),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["bin_only_single_test"])
    }

    #[rudzio::test]
    fn bin_only_member_aggregates_two_flat_test_files(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["mybin"])?;

        let mybin = root.join("mybin");
        fs::create_dir_all(mybin.join("src"))?;
        fs::create_dir_all(mybin.join("tests"))?;
        write_bin_only_manifest(&mybin, "mybin", &[("a", "tests/a.rs"), ("b", "tests/b.rs")])?;
        fs::write(mybin.join("src").join("main.rs"), "fn main() {}\n")?;
        fs::write(
            mybin.join("tests").join("a.rs"),
            integration_test_source("flat_a_test", "anyhow::ensure!(1 + 1 == 2);"),
        )?;
        fs::write(
            mybin.join("tests").join("b.rs"),
            integration_test_source("flat_b_test", "anyhow::ensure!(3 + 3 == 6);"),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["flat_a_test", "flat_b_test"])
    }

    #[rudzio::test]
    fn bin_only_member_aggregates_directory_test_with_helper(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["mybin"])?;

        let mybin = root.join("mybin");
        fs::create_dir_all(mybin.join("src"))?;
        fs::create_dir_all(mybin.join("tests").join("x"))?;
        write_bin_only_manifest(&mybin, "mybin", &[("x", "tests/x/mod.rs")])?;
        fs::write(mybin.join("src").join("main.rs"), "fn main() {}\n")?;

        // The test root declares `mod helper;` (a code module, not a test
        // binary) and references it via `crate::helper::answer()`. The
        // proxy hoists the helper and rewrites `crate::helper` to the
        // synthesised module name.
        fs::write(
            mybin.join("tests").join("x").join("mod.rs"),
            "// rudzio_test marker\n\
             #![allow(clippy::tests_outside_test_module)]\n\
             \n\
             mod helper;\n\
             \n\
             use rudzio::common::context::{Suite, Test};\n\
             use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
             \n\
             #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
             mod tests {\n\
             \x20\x20\x20\x20use crate::helper::answer;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn directory_test_root_uses_helper() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42);\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}\n\
             }\n\
             \n\
             #[rudzio::main]\n\
             fn main() {}\n",
        )?;
        fs::write(
            mybin.join("tests").join("x").join("helper.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["directory_test_root_uses_helper"])
    }

    #[rudzio::test]
    fn bin_only_member_resolves_cargo_bin_exe(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["mybin"])?;

        let mybin = root.join("mybin");
        fs::create_dir_all(mybin.join("src"))?;
        fs::create_dir_all(mybin.join("tests"))?;
        write_bin_only_manifest(&mybin, "mybin", &[("binexe", "tests/binexe.rs")])?;
        fs::write(mybin.join("src").join("main.rs"), "fn main() {}\n")?;
        fs::write(
            mybin.join("tests").join("binexe.rs"),
            integration_test_source(
                "bin_exe_env_resolves",
                "let bin_path = env!(\"CARGO_BIN_EXE_mybin\");\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(!bin_path.is_empty());",
            ),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["bin_exe_env_resolves"])
    }

    #[rudzio::test]
    fn bin_only_member_with_bin_source_tests_runs_both_sets_once(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["mybin"])?;

        let mybin = root.join("mybin");
        fs::create_dir_all(mybin.join("src"))?;
        fs::create_dir_all(mybin.join("tests"))?;
        write_bin_only_manifest(&mybin, "mybin", &[("integration", "tests/integration.rs")])?;
        // The default bin's `src/main.rs` carries its own `#[rudzio::test]`
        // (Bug #19 path); the integration test is a separate proxy. Both
        // must run exactly once — the summary count guards against any
        // double-registration.
        fs::write(
            mybin.join("src").join("main.rs"),
            bin_source_with_test_source("bin_source_test"),
        )?;
        fs::write(
            mybin.join("tests").join("integration.rs"),
            integration_test_source("integration_test", "anyhow::ensure!(2 + 2 == 4);"),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["bin_source_test", "integration_test"])
    }

    #[rudzio::test]
    fn workspace_bin_only_no_test_member_does_not_panic(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["noop", "tested"])?;

        // `noop`: bin-only member with zero rudzio surface anywhere (no
        // `[lib]`, no test markers in `src/main.rs`, no `tests/`).
        let noop = root.join("noop");
        fs::create_dir_all(noop.join("src"))?;
        write_bin_only_manifest(&noop, "noop", &[])?;
        fs::write(noop.join("src").join("main.rs"), "fn main() {}\n")?;

        // `tested`: bin-only member with one integration test. Its test
        // must run; `noop`'s empty stub must not cause a linkme panic.
        let tested = root.join("tested");
        fs::create_dir_all(tested.join("src"))?;
        fs::create_dir_all(tested.join("tests"))?;
        write_bin_only_manifest(
            &tested,
            "tested",
            &[("integration", "tests/integration.rs")],
        )?;
        fs::write(tested.join("src").join("main.rs"), "fn main() {}\n")?;
        fs::write(
            tested.join("tests").join("integration.rs"),
            integration_test_source("tested_member_test", "anyhow::ensure!(2 + 2 == 4);"),
        )?;

        // Workspace-wide (no `-p`): `noop` contributes nothing, `tested`
        // contributes exactly one test.
        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["tested_member_test"])
    }
}
