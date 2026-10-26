//! Proactive edge-case e2e coverage for cargo-rudzio's self-named-crate
//! handling and crate-path resolution (Bug #7 family, post-fix).
//!
//! Bug #7 lived in exactly this seam: a lib-bearing member whose own
//! `src/lib.rs` declares a root-level item named after the crate
//! (`extern crate self as X;` colliding `mod X;`, E0260). The fix routes
//! such members through a `__rudzio_self_alias` module
//! (`pub(crate) use crate as X;`) glob-imported into every proxied test
//! module, and rewrites explicit `::X::` paths to `crate::`. This spec
//! probes the edges that remain after that fix: non-default module paths,
//! cfg-gating, BOM / CRLF source bytes, raw identifiers, a `tests/main.rs`
//! shim, every spelling of a crate-path reference, lib-name overrides,
//! cross-crate self-named dependencies, and a coexisting bin target.
//!
//! Every case is a real consumer workspace (`crates/*`, resolver 2,
//! `file://` git rudzio dep via `workspace = true`, runtime-futures,
//! `warnings = "deny"` + `check-cfg = ['cfg(rudzio_test)']`,
//! `panic = "abort"`, `.cargo/config.toml` build-dir) driven by the
//! freshness-checked cargo-rudzio binary.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, assert_fresh_binary, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread;

/// The `#[rudzio::suite]` preamble shared by every fixture test file:
/// futures `ThreadPool` runtime (matches the fixture's `runtime-futures`
/// feature) plus the `Suite`/`Test` context types.
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

/// The checkout's real HEAD SHA, used as the `rev` pin so the git
/// dependency builds the rudzio code under test rather than a published
/// version.
fn rudzio_git_rev() -> anyhow::Result<String> {
    current_git_head()
        .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))
}

/// The absolute `file://` git URL for the rudzio repo this test lives in.
fn rudzio_git_url() -> String {
    format!("file://{}", repo_root().display())
}

/// Write the operator-shaped workspace root manifest: `crates/*`, resolver
/// 2, `file://` git rudzio dep (runtime-futures), `warnings = "deny"` +
/// `check-cfg = ['cfg(rudzio_test)']`, panic-abort profiles, and the
/// `.cargo/config.toml` build-dir.
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

/// Write a lib-bearing member manifest: package `package_name` with
/// `[lib] name = lib_name` and a single flat `tests/integration/mod.rs`
/// harness-false `[[test]]`.
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

/// Write a suite-only integration test source (no `#[rudzio::main]`), for
/// shapes where a sibling `tests/main.rs` shim already carries the
/// `#[rudzio::main]` entry point (Bug #10 shim shape).
fn write_suite_only_source(path: &Path, body: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{SUITE_HEADER}{body}}}\n"))
}

/// The common single-test member body: import `answer` via `import_line`
/// and assert it equals 42.
fn simple_body(import_line: &str, test_name: &str) -> String {
    format!(
        "    {import_line}\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20async fn {test_name}() -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42, \"expected answer() == 42\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}}\n"
    )
}

/// Materialise a self-contained lib member under `workspace/crates/<package>`
/// with `lib_src` as `src/lib.rs`, `module_files` as extra `src/` files,
/// and `test_body` as the integration test body.
fn write_member(
    workspace: &Path,
    package_name: &str,
    lib_name: &str,
    lib_src: &str,
    module_files: &[(&str, &str)],
    test_body: &str,
) -> io::Result<()> {
    let member = workspace.join("crates").join(package_name);
    fs::create_dir_all(member.join("src"))?;
    fs::create_dir_all(member.join("tests").join("integration"))?;
    write_lib_member_manifest(&member, package_name, lib_name)?;
    fs::write(member.join("src").join("lib.rs"), lib_src)?;
    for (rel, content) in module_files {
        fs::write(member.join("src").join(rel), content)?;
    }
    write_test_source(&member.join("tests").join("integration"), test_body)?;
    Ok(())
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
        "runner must report exactly {} passed, but stdout did not contain \"{} passed\".\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        expected.len(),
        expected.len(),
    );
    Ok(())
}

#[rudzio::suite([(runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, assert_exact_passed, cargo_build_dir_for, fs, rudzio_git_rev,
        run_cargo_rudzio, simple_body, write_member, write_suite_only_source, write_test_source,
        write_workspace_manifest,
    };

    /// The `answer() -> 42` self-named module body shared by most cases.
    const ANSWER_RS: &str = "pub fn answer() -> u32 {\n    42\n}\n";

    /// **Control** — a crate with no self-named root item keeps the
    /// `extern crate self as plain;` alias and `use plain::answer;`
    /// resolves by name through it.
    #[rudzio::test]
    async fn control_non_self_named_alias_resolves(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("control-non-self-named");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "plain",
            "plain",
            "pub fn answer() -> u32 {\n    42\n}\n",
            &[],
            &simple_body("use plain::answer;", "control_non_self_named_answer"),
        )?;
        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["control_non_self_named_answer"])
    }

    /// **`#[path]` attribute** — a self-named module declared via a
    /// non-default path (`#[path = "alt.rs"] mod project;`) is detected and
    /// `use project::project::answer;` resolves.
    #[rudzio::test]
    async fn self_named_path_attr_module_resolves(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("path-attr");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "project",
            "project",
            "#[path = \"alt.rs\"]\npub mod project;\n",
            &[("alt.rs", ANSWER_RS)],
            &simple_body(
                "use project::project::answer;",
                "path_attr_self_named_answer",
            ),
        )?;
        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["path_attr_self_named_answer"])
    }

    /// **cfg-gated self-named item** — `#[cfg(not(rudzio_test))] mod project;`
    /// means the self-named module is absent under `cfg(rudzio_test)`, so the
    /// crate name must still resolve (via the alias) to the root `answer`.
    #[rudzio::test]
    async fn self_named_cfg_gated_module_uses_alias(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("cfg-gated");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "project",
            "project",
            "#[cfg(not(rudzio_test))]\nmod project;\n\npub fn answer() -> u32 {\n    42\n}\n",
            &[("project.rs", ANSWER_RS)],
            &simple_body("use project::answer;", "cfg_gated_self_named_answer"),
        )?;
        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["cfg_gated_self_named_answer"])
    }

    /// **UTF-8 BOM** — a `src/lib.rs` that begins with a UTF-8 byte-order
    /// mark is still scanned for the self-named module, so the alias skip
    /// and `__rudzio_self_alias` glob apply.
    #[rudzio::test]
    async fn self_named_bom_lib_detects(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("bom-lib");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "project",
            "project",
            "\u{feff}pub mod project;\n",
            &[("project.rs", ANSWER_RS)],
            &simple_body("use project::project::answer;", "bom_self_named_answer"),
        )?;
        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["bom_self_named_answer"])
    }

    /// **CRLF line endings** — a `src/lib.rs` whose lines end in `\r\n` is
    /// still detected and the self-named path resolves.
    #[rudzio::test]
    async fn self_named_crlf_lib_detects(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("crlf-lib");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "project",
            "project",
            "pub mod project;\r\n",
            &[("project.rs", ANSWER_RS)],
            &simple_body("use project::project::answer;", "crlf_self_named_answer"),
        )?;
        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["crlf_self_named_answer"])
    }

    /// **Raw identifier** — a crate root that also carries a raw-ident item
    /// (`pub fn r#type()`) alongside the self-named module does not
    /// false-positive the self-named detection, and the self-named path
    /// still resolves.
    #[rudzio::test]
    async fn self_named_raw_ident_no_false_positive(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("raw-ident");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "project",
            "project",
            "pub fn r#type() -> u32 {\n    1\n}\n\npub mod project;\n",
            &[("project.rs", ANSWER_RS)],
            &simple_body(
                "use project::project::answer;",
                "raw_ident_self_named_answer",
            ),
        )?;
        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["raw_ident_self_named_answer"])
    }

    /// **`tests/main.rs` shim** — a self-named member whose `[[test]]` entry
    /// is a `tests/main.rs` shim (`mod integration; #[rudzio::main]`) with
    /// the test surface in `tests/integration.rs`. Combines the Bug #10
    /// shim shape with Bug #7's self-named handling.
    #[rudzio::test]
    async fn self_named_tests_main_rs_shim_aggregates(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("main-rs-shim");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;

        let member = workspace.join("crates").join("project");
        fs::create_dir_all(member.join("src"))?;
        fs::create_dir_all(member.join("tests"))?;
        fs::write(
            member.join("Cargo.toml"),
            "[package]\n\
             name = \"project\"\n\
             version = \"0.1.0\"\n\
             edition.workspace = true\n\
             publish = false\n\
             autotests = false\n\
             \n\
             [lib]\n\
             name = \"project\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [[test]]\n\
             name = \"main\"\n\
             path = \"tests/main.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true }\n\
             anyhow = { workspace = true }\n\
             \n\
             [lints]\n\
             workspace = true\n",
        )?;
        fs::write(member.join("src").join("lib.rs"), "pub mod project;\n")?;
        fs::write(member.join("src").join("project.rs"), ANSWER_RS)?;
        fs::write(
            member.join("tests").join("main.rs"),
            "mod integration;\n\n#[rudzio::main]\nfn main() {}\n",
        )?;
        write_suite_only_source(
            &member.join("tests").join("integration.rs"),
            &simple_body("use project::project::answer;", "shim_self_named_answer"),
        )?;

        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["shim_self_named_answer"])
    }

    /// **Self-named macro** — a `macro_rules! project` at the crate root is a
    /// self-named binding; the alias skip applies and `use project::answer;`
    /// still resolves the crate root.
    #[rudzio::test]
    async fn self_named_macro_rules_root_resolves(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("macro-root");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "project",
            "project",
            "pub fn answer() -> u32 {\n    42\n}\n\nmacro_rules! project {\n    () => {};\n}\n",
            &[],
            &simple_body("use project::answer;", "macro_self_named_answer"),
        )?;
        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["macro_self_named_answer"])
    }

    /// **Every crate-path spelling** — a self-named crate `x` (root `foo()`
    /// plus self-named module `x::{item, Marker}`) referenced by an
    /// integration test in every form: bare `use`, `x::foo()`,
    /// `::x::foo()` (leading colon), `crate::foo()`, `x::x::item()`, and a
    /// type position.
    #[rudzio::test]
    async fn integration_refers_crate_every_form(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("every-form");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;

        let x_rs = "pub fn item() -> u32 {\n    7\n}\n\npub struct Marker;\n";
        let body = "    use x::foo;\n\
                    \x20\x20\x20\x20use x::x::{Marker, item};\n\
                    \n\
                    \x20\x20\x20\x20#[rudzio::test]\n\
                    \x20\x20\x20\x20async fn every_form_references_crate() -> anyhow::Result<()> {\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(foo() == 42, \"via bare use\");\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(x::foo() == 42, \"via x::foo\");\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20let _leading_colon: u32 = ::x::foo();\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(crate::foo() == 42, \"via crate::foo\");\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(item() == 7, \"via x::x::item use\");\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(x::x::item() == 7, \"via x::x::item path\");\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20let _m: Marker = x::x::Marker;\n\
                    \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
                    \x20\x20\x20\x20}\n";

        write_member(
            &workspace,
            "x",
            "x",
            "pub mod x;\npub fn foo() -> u32 {\n    42\n}\n",
            &[("x.rs", x_rs)],
            body,
        )?;

        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["every_form_references_crate"])
    }

    /// **Hyphenated package, lib name overridden** — package `my-crate` with
    /// `[lib] name = "renamed_lib"` and a self-named `mod renamed_lib;`.
    /// The test references the crate by its real lib name
    /// (`renamed_lib::renamed_lib::foo`), which cargo-rudzio must resolve by
    /// the lib name, not the package name.
    #[rudzio::test]
    async fn hyphenated_package_lib_name_override(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("lib-name-override");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;
        write_member(
            &workspace,
            "my-crate",
            "renamed_lib",
            "pub mod renamed_lib;\n",
            &[("renamed_lib.rs", "pub fn foo() -> u32 {\n    42\n}\n")],
            "    use renamed_lib::renamed_lib::foo;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20async fn hyphenated_lib_name_override_answer() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(foo() == 42, \"expected foo() == 42\");\n\
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
        assert_exact_passed(&output, &["hyphenated_lib_name_override_answer"])
    }

    /// **Control — hyphenated package, no `[lib] name` override** — package
    /// `my-crate` with the implicit default lib name `my_crate`. The test
    /// references the crate by its sanitised lib name (`my_crate::answer`),
    /// proving a hyphenated package whose lib name matches the sanitised
    /// package name still resolves (the fix must not regress the common
    /// package-name == lib-name path).
    #[rudzio::test]
    async fn hyphenated_package_default_lib_name(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("hyphenated-default-lib-name");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;

        let member = workspace.join("crates").join("my-crate");
        fs::create_dir_all(member.join("src"))?;
        fs::create_dir_all(member.join("tests").join("integration"))?;
        fs::write(
            member.join("Cargo.toml"),
            "[package]\n\
             name = \"my-crate\"\n\
             version = \"0.1.0\"\n\
             edition.workspace = true\n\
             publish = false\n\
             autotests = false\n\
             \n\
             [lib]\n\
             path = \"src/lib.rs\"\n\
             \n\
             [[test]]\n\
             name = \"integration\"\n\
             path = \"tests/integration/mod.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true }\n\
             anyhow = { workspace = true }\n\
             \n\
             [lints]\n\
             workspace = true\n",
        )?;
        fs::write(
            member.join("src").join("lib.rs"),
            "pub fn answer() -> u32 {\n    42\n}\n",
        )?;
        write_test_source(
            &member.join("tests").join("integration"),
            &simple_body(
                "use my_crate::answer;",
                "hyphenated_default_lib_name_answer",
            ),
        )?;

        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &workspace,
            &[],
        )?;
        assert_exact_passed(&output, &["hyphenated_default_lib_name_answer"])
    }

    /// **Cross-crate self-named dependency** — crate `a` depends on the
    /// self-named crate `b` (whose lib declares `pub mod b;`), and `a`'s
    /// test uses `b::b::item`. Run workspace-wide.
    #[rudzio::test]
    async fn workspace_self_named_dep_cross_crate(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("cross-crate");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;

        // Self-named dependency `b`: `pub mod b;` -> `b::b::item`.
        let dep_b = workspace.join("crates").join("b");
        fs::create_dir_all(dep_b.join("src"))?;
        fs::write(
            dep_b.join("Cargo.toml"),
            "[package]\n\
             name = \"b\"\n\
             version = \"0.1.0\"\n\
             edition.workspace = true\n\
             publish = false\n\
             \n\
             [lib]\n\
             name = \"b\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [lints]\n\
             workspace = true\n",
        )?;
        fs::write(dep_b.join("src").join("lib.rs"), "pub mod b;\n")?;
        fs::write(
            dep_b.join("src").join("b.rs"),
            "pub fn item() -> u32 {\n    42\n}\n",
        )?;

        // Consumer `a`: depends on `b` via path; its test uses `b::b::item`.
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
             [dependencies]\n\
             b = { path = \"../b\" }\n\
             \n\
             [[test]]\n\
             name = \"integration\"\n\
             path = \"tests/integration/mod.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
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
            "    use b::b::item;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20async fn cross_crate_self_named_item() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(item() == 42, \"expected item() == 42\");\n\
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
        assert_exact_passed(&output, &["cross_crate_self_named_item"])
    }

    /// **Self-named crate + bin target** — a self-named lib crate `x` that
    /// also ships a `src/main.rs` bin doing `use x::x::foo;`. The bin's
    /// reference to its own self-named lib must resolve alongside the lib's
    /// integration test.
    #[rudzio::test]
    async fn self_named_crate_with_bin_target(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("with-bin");
        let git_rev = rudzio_git_rev()?;
        write_workspace_manifest(&workspace, &git_rev)?;

        let member = workspace.join("crates").join("x");
        fs::create_dir_all(member.join("src"))?;
        fs::create_dir_all(member.join("tests").join("integration"))?;
        fs::write(
            member.join("Cargo.toml"),
            "[package]\n\
             name = \"x\"\n\
             version = \"0.1.0\"\n\
             edition.workspace = true\n\
             publish = false\n\
             autotests = false\n\
             \n\
             [lib]\n\
             name = \"x\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [[bin]]\n\
             name = \"x\"\n\
             path = \"src/main.rs\"\n\
             \n\
             [[test]]\n\
             name = \"integration\"\n\
             path = \"tests/integration/mod.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true }\n\
             anyhow = { workspace = true }\n\
             \n\
             [lints]\n\
             workspace = true\n",
        )?;
        fs::write(member.join("src").join("lib.rs"), "pub mod x;\n")?;
        fs::write(
            member.join("src").join("x.rs"),
            "pub fn foo() -> u32 {\n    42\n}\n",
        )?;
        fs::write(
            member.join("src").join("main.rs"),
            "use x::x::foo;\n\nfn main() {\n    println!(\"{}\", foo());\n}\n",
        )?;
        write_test_source(
            &member.join("tests").join("integration"),
            "    use x::x::foo;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20async fn bin_coexists_self_named_answer() -> anyhow::Result<()> {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(foo() == 42, \"expected foo() == 42\");\n\
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
        assert_exact_passed(&output, &["bin_coexists_self_named_answer"])
    }
}
