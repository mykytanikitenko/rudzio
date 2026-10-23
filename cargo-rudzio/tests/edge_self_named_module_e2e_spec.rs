//! End-to-end edge-case coverage for Bug #7: a lib-bearing member whose
//! own crate root declares an item named after the crate (a "self-named"
//! binding) cannot take the bridge's `extern crate self as <name>;`
//! prelude entry — it collides (E0260).
//!
//! # Fix shape (on main)
//!
//! `crate_root_declares_ident` detects the self-named binding, the bridge
//! skips the self-alias, and `CratePathRewriter` rewrites `<name>::` paths
//! in the proxied test sources to `crate::`. The cases below probe the
//! breadth of that detection: every `syn::Item` variant that binds a name,
//! hyphenated crate names, the alias-preserving control, and two forms the
//! `syn`-driven rewriter cannot reach (paths inside macro tokens, and a
//! leading-`::` `use`).
//!
//! # Assertion strategy
//!
//! Each case runs `cargo rudzio test` and asserts the exit status plus the
//! exact set of passed test names (summary `N passed;` == expected count
//! AND every expected name present in stdout).

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/`; the fixture's `rudzio` path dependency.
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

/// Write a lib-bearing member's `Cargo.toml`. `crate_name` is the package
/// name (may contain `-`); `lib_name` is the `[lib] name` (the Rust ident).
fn write_lib_member_manifest(
    member_dir: &Path,
    crate_name: &str,
    lib_name: &str,
) -> io::Result<()> {
    fs::write(
        member_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"{crate_name}\"\n\
             version = \"0.1.0\"\n\
             edition = \"2024\"\n\
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
             anyhow = {{ workspace = true }}\n"
        ),
    )
}

/// A rudzio integration-test source with a single `#[rudzio::test]` named
/// `test_name`. `import_line` is the `use` emitted inside `mod tests`
/// (typically `use <crate>::<crate>::answer;`). The body asserts
/// `answer() == 42`.
fn self_named_test_source(test_name: &str, import_line: &str) -> String {
    format!(
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         use rudzio::common::context::{{Suite, Test}};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {{\n\
         \x20\x20\x20\x20{import_line}\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn {test_name}() -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42, \"expected answer() == 42\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}}\n\
         }}\n\
         \n\
         #[rudzio::main]\n\
         fn main() {{}}\n",
    )
}

/// Spawn `cargo rudzio test` against `root` with the isolated cargo env.
fn run_cargo_rudzio(
    root: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    args: &[&str],
) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
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

/// Assert exit 0 and that exactly `expected` is the passed test set:
/// summary count equals `expected.len()` and every name is in stdout.
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
        IsolatedCargoTest, Path, TempDir, assert_exact_passed, fs, run_cargo_rudzio,
        self_named_test_source, write_lib_member_manifest, write_workspace_manifest,
    };

    #[rudzio::test]
    fn self_named_module_skips_alias_and_rewrites(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["project"])?;

        let project = root.join("project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests").join("integration"))?;
        write_lib_member_manifest(&project, "project", "project")?;
        fs::write(project.join("src").join("lib.rs"), "pub mod project;\n")?;
        fs::write(
            project.join("src").join("project.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        fs::write(
            project.join("tests").join("integration").join("mod.rs"),
            self_named_test_source("self_named_module_answer", "use project::project::answer;"),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["self_named_module_answer"])
    }

    #[rudzio::test]
    fn self_named_struct_skips_alias_and_rewrites(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["project"])?;

        let project = root.join("project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests").join("integration"))?;
        write_lib_member_manifest(&project, "project", "project")?;
        fs::write(
            project.join("src").join("lib.rs"),
            "pub struct project {\n    pub answer: u32,\n}\n",
        )?;
        fs::write(
            project.join("tests").join("integration").join("mod.rs"),
            "// rudzio_test marker\n\
             #![allow(clippy::tests_outside_test_module)]\n\
             \n\
             use rudzio::common::context::{Suite, Test};\n\
             use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
             \n\
             #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
             mod tests {\n\
             \x20\x20\x20\x20use project::project;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn struct_self_named_resolves() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20let p = project { answer: 42 };\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(p.answer == 42);\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}\n\
             }\n\
             \n\
             #[rudzio::main]\n\
             fn main() {}\n",
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["struct_self_named_resolves"])
    }

    #[rudzio::test]
    fn self_named_use_binding_skips_alias_and_rewrites(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["project"])?;

        let project = root.join("project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests").join("integration"))?;
        write_lib_member_manifest(&project, "project", "project")?;
        // The crate root binds `project` via `pub use other::project;`
        // (a `UseTree::Path`, not a bare name). The self-alias would
        // collide with this binding.
        fs::write(
            project.join("src").join("lib.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n\n\
             mod other {\n\
             \x20\x20\x20\x20pub struct project;\n\
             }\n\
             pub use other::project;\n",
        )?;
        fs::write(
            project.join("tests").join("integration").join("mod.rs"),
            self_named_test_source("use_binding_self_named_answer", "use project::answer;"),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["use_binding_self_named_answer"])
    }

    #[rudzio::test]
    fn self_named_macro_import_rewrites(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["project"])?;

        let project = root.join("project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests").join("integration"))?;
        write_lib_member_manifest(&project, "project", "project")?;
        fs::write(project.join("src").join("lib.rs"), "pub mod project;\n")?;
        fs::write(
            project.join("src").join("project.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        // The `use project::project::answer;` lives inside a `macro_rules!`
        // body. The `syn`-driven rewriter cannot reach paths inside macro
        // tokens, so the `<project>::` → `crate::` rewrite does not apply.
        fs::write(
            project.join("tests").join("integration").join("mod.rs"),
            "// rudzio_test marker\n\
             #![allow(clippy::tests_outside_test_module)]\n\
             \n\
             use rudzio::common::context::{Suite, Test};\n\
             use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
             \n\
             #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
             mod tests {\n\
             \x20\x20\x20\x20macro_rules! import_answer {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20() => { use project::project::answer; };\n\
             \x20\x20\x20\x20}\n\
             \x20\x20\x20\x20import_answer!();\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn macro_import_answer_resolves() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42, \"expected answer() == 42\");\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}\n\
             }\n\
             \n\
             #[rudzio::main]\n\
             fn main() {}\n",
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["macro_import_answer_resolves"])
    }

    #[rudzio::test]
    fn hyphenated_crate_self_named_module_rewrites(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["my-project"])?;

        let project = root.join("my-project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests").join("integration"))?;
        write_lib_member_manifest(&project, "my-project", "my_project")?;
        fs::write(project.join("src").join("lib.rs"), "pub mod my_project;\n")?;
        fs::write(
            project.join("src").join("my_project.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        fs::write(
            project.join("tests").join("integration").join("mod.rs"),
            self_named_test_source(
                "hyphenated_self_named_answer",
                "use my_project::my_project::answer;",
            ),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["hyphenated_self_named_answer"])
    }

    #[rudzio::test]
    fn non_self_named_control_still_uses_alias(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["project"])?;

        let project = root.join("project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests").join("integration"))?;
        write_lib_member_manifest(&project, "project", "project")?;
        // No root-level item named `project` — the bridge keeps its
        // `extern crate self as project;` alias, and the test resolves the
        // crate by name through it.
        fs::write(
            project.join("src").join("lib.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        fs::write(
            project.join("tests").join("integration").join("mod.rs"),
            self_named_test_source("control_alias_answer", "use project::answer;"),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["control_alias_answer"])
    }

    #[rudzio::test]
    fn leading_colon_use_rewrites(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["project"])?;

        let project = root.join("project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests").join("integration"))?;
        write_lib_member_manifest(&project, "project", "project")?;
        fs::write(project.join("src").join("lib.rs"), "pub mod project;\n")?;
        fs::write(
            project.join("src").join("project.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        fs::write(
            project.join("tests").join("integration").join("mod.rs"),
            self_named_test_source("leading_colon_answer", "use ::project::project::answer;"),
        )?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exact_passed(&output, &["leading_colon_answer"])
    }
}
