//! Faithful e2e reproducer for the **unfixed** half of Bug #10 against the
//! operator's exact `lexer` crate shape: a lib member in a `crates/*` workspace
//! whose `[[test]]` entry is a `tests/main.rs` shim that declares the crate-root
//! `mod X;` decls (`mod asm_common; mod code; mod cursor; mod harness;`), with
//! the shared helper living at `tests/harness/mod.rs` + `tests/harness/helpers.rs`
//! and a non-Rust `include_str!("std-numeric.me")` fixture.
//!
//! This complements [`super::migration_bug_10_lexer_harness_e2e_spec`], which
//! covers the flat-`tests/integration.rs`-declares-`mod harness;` shape. The
//! operator's real `lexer` is different in the load-bearing detail that makes
//! THIS bug distinct: `mod harness;` is declared **only** in the `tests/main.rs`
//! shim (the `[[test]]` crate root), and each sibling test file imports the
//! helper *through the crate root* without declaring `mod harness;` itself:
//!
//! ```ignore
//! // tests/main.rs (shim)
//! mod asm_common;
//! mod code;
//! mod cursor;
//! mod harness;
//! #[rudzio::main] fn main() {}
//!
//! // tests/asm_common.rs, tests/code.rs, tests/cursor.rs (each)
//! #[rudzio::suite([...])]
//! mod tests {
//!     use crate::harness::{lex, tok};
//!     ...
//! }
//! ```
//!
//! # Symptom (pre-fix, a70456d)
//!
//! ```text
//! error[E0432]: unresolved import `crate::harness`   (×9)
//! error[E0433]: cannot find `harness` in the crate root   (×1)
//! could not compile `lexer_rudzio_bridge` (lib) due to 10 previous errors
//! ```
//!
//! # Root cause
//!
//! `discover_test_files` excludes the `tests/main.rs` shim from `test_files`
//! (it carries only `#[rudzio::main]`, no test surface), so the shim's own
//! `mod harness;` decl is never passed through `discover_test_root_helpers`.
//! The sibling test files (which only `use crate::harness::…`) therefore have no
//! helper mounted at the bridge crate root, and their `crate::harness` path is
//! never rewritten. The earlier #10 fix (94e2ddd) only discovered helpers
//! declared *next to* test files; helpers declared only in the shim were dropped.
//!
//! # Fix
//!
//! Carry every non-test `mod X;` declared in the `[[test]]` entry shim into the
//! bridge for every proxy of the member, under per-binary synthesised names so
//! `crate::harness` resolves (`crate::__rudzio__<binary>__harness`).

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, assert_fresh_binary, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread;

/// The checkout's real HEAD SHA, used as the `rev` pin so the git dependency
/// builds the rudzio code under test rather than a published version.
fn rudzio_git_rev() -> anyhow::Result<String> {
    current_git_head()
        .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))
}

/// The absolute `file://` git URL for the rudzio repo this test lives in.
fn rudzio_git_url() -> String {
    format!("file://{}", repo_root().display())
}

/// Materialise the operator's exact `lexer` workspace under `parent/workspace`.
fn write_fixture(parent: &Path, rev: &str) -> io::Result<PathBuf> {
    let root = parent.join("workspace");
    let lexer = root.join("crates").join("lexer");
    fs::create_dir_all(lexer.join("src"))?;
    fs::create_dir_all(lexer.join("tests").join("harness"))?;
    fs::create_dir_all(root.join(".cargo"))?;

    // Consumer's own `.cargo/config.toml` (build-dir only).
    fs::write(
        root.join(".cargo").join("config.toml"),
        "[build]\nbuild-dir = \"tmp/build-dir\"\n",
    )?;

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
             rudzio = {{ git = \"{git_url}\", rev = \"{rev}\", features = [\"runtime-futures\", \"runtime-compio\", \"common\"] }}\n\
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
             panic = \"abort\"\n"
        ),
    )?;

    fs::write(
        lexer.join("Cargo.toml"),
        "[package]\n\
         name = \"lexer\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"lexer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[test]]\n\
         name = \"main\"\n\
         path = \"tests/main.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n",
    )?;

    fs::write(
        lexer.join("src").join("lib.rs"),
        "//! Minimal lexer so the fixture compiles under `warnings = \"deny\"`.\n\
         \n\
         /// Split `input` on ASCII whitespace.\n\
         pub fn split(input: &str) -> Vec<&str> {\n\
         \x20\x20\x20\x20input.split_ascii_whitespace().collect()\n\
         }\n",
    )?;

    // The `[[test]]` crate root: declares the sibling test modules AND the
    // shared `harness` helper module. This is the only place `mod harness;`
    // appears — exactly the operator's shape that the pre-fix emitter drops.
    fs::write(
        lexer.join("tests").join("main.rs"),
        "mod asm_common;\n\
         mod code;\n\
         mod cursor;\n\
         mod harness;\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;

    fs::write(
        lexer.join("tests").join("harness").join("mod.rs"),
        "//! Shared test helpers.\n\
         \n\
         pub(crate) mod helpers;\n\
         \n\
         pub(crate) use helpers::{lex, tok};\n",
    )?;

    fs::write(
        lexer.join("tests").join("harness").join("helpers.rs"),
        "//! Helper implementations.\n\
         \n\
         /// One lexed item.\n\
         #[derive(Debug, PartialEq, Eq)]\n\
         pub(crate) enum Item<'src> {\n\
         \x20\x20\x20\x20/// A word token.\n\
         \x20\x20\x20\x20Word(&'src str),\n\
         }\n\
         \n\
         /// Lex `input` into items.\n\
         pub(crate) fn lex(input: &str) -> anyhow::Result<Vec<Item<'_>>> {\n\
         \x20\x20\x20\x20Ok(lexer::split(input).into_iter().map(Item::Word).collect())\n\
         }\n\
         \n\
         /// Build a word item.\n\
         pub(crate) const fn tok(word: &str) -> Item<'_> {\n\
         \x20\x20\x20\x20Item::Word(word)\n\
         }\n",
    )?;

    // Non-Rust fixture read via `include_str!` from one test file.
    fs::write(lexer.join("tests").join("std-numeric.me"), "42 7\n")?;

    for (module, extra_test) in [("asm_common", false), ("code", true), ("cursor", false)] {
        let numeric_block = if extra_test {
            "\n    const STD_NUMERIC: &str = include_str!(\"std-numeric.me\");\n\n    #[rudzio::test]\n    async fn code_lexes_std_numeric(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {\n        anyhow::ensure!(lex(STD_NUMERIC)? == vec![tok(\"42\"), tok(\"7\")]);\n        Ok(())\n    }\n"
        } else {
            ""
        };
        let body = format!(
            "//! `{module}` lexer tests.\n\
             \n\
             #[rudzio::suite([(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test)])]\n\
             mod tests {{\n\
             \x20\x20\x20\x20use crate::harness::{{lex, tok}};{numeric_block}\n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20async fn {module}_lexes_words(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(lex(\"a b\")? == vec![tok(\"a\"), tok(\"b\")]);\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}}\n\
             }}\n"
        );
        fs::write(lexer.join("tests").join(format!("{module}.rs")), body)?;
    }

    Ok(root)
}

/// Warm the per-test cargo home's git cache for the fixture's `file://` git
/// dependency, then the fixture is run offline by the caller. `--offline`
/// blocks ALL git operations (including local `file://` clones), so the git
/// dependency must be fetched before the offline run.
fn prepopulate_git_dep(cargo_home: &Path, fixture_root: &Path) -> anyhow::Result<()> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo_bin)
        .arg("fetch")
        .arg("--manifest-path")
        .arg(fixture_root.join("Cargo.toml"))
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_NET_OFFLINE", "false")
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(io::Error::other)?;
    anyhow::ensure!(
        output.status.success(),
        "pre-fetching the git dependency for {} failed: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        fixture_root.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// Drive `cargo rudzio test <args>` against `fixture_root` with the
/// freshness-checked cargo-rudzio binary and an offline (pre-warmed) cargo
/// home. The inner build writes to per-test target/build dirs.
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
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(io::Error::other)?;
    Ok(output)
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Output;

    use super::{
        IsolatedCargoTest, prepopulate_git_dep, rudzio_git_rev, run_cargo_rudzio, write_fixture,
    };

    /// The four `#[rudzio::test]` fns the operator's sibling test files declare
    /// (`asm_common_lexes_words`, `code_lexes_words`, `code_lexes_std_numeric`,
    /// `cursor_lexes_words`).
    const EXPECTED_TESTS: [&str; 4] = [
        "asm_common_lexes_words",
        "code_lexes_words",
        "code_lexes_std_numeric",
        "cursor_lexes_words",
    ];

    /// Assert a successful `cargo rudzio test` run reported exactly the four
    /// lexer tests.
    fn assert_lexer_tests(output: &Output) -> anyhow::Result<()> {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::ensure!(
            output.status.success(),
            "lexer member must run its shim-declared harness tests (exit 0), but the runner \
             exited {:?}.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code(),
        );
        for expected in EXPECTED_TESTS {
            anyhow::ensure!(
                stdout.contains(expected),
                "runner stdout must name the aggregated test `{expected}`.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            );
        }
        anyhow::ensure!(
            stdout.contains("4 passed"),
            "runner must report exactly 4 passing tests, but stdout did not contain \"4 passed\".\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
        Ok(())
    }

    /// Build a task-local cargo build dir beside the cargo target dir.
    fn cargo_build_dir_for(cargo_target_dir: &Path) -> anyhow::Result<PathBuf> {
        Ok(cargo_target_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir"))
    }

    /// Drive the fixture once (prepopulate + run), returning the output.
    fn drive(
        cargo_home: &Path,
        cargo_target_dir: &Path,
        suite_tempdir: &Path,
        args: &[&str],
    ) -> anyhow::Result<Output> {
        let rev = rudzio_git_rev()?;
        let parent = suite_tempdir.join("lexer-harness");
        let fixture_root = write_fixture(&parent, &rev)?;
        let cargo_build_dir = cargo_build_dir_for(cargo_target_dir)?;
        prepopulate_git_dep(cargo_home, &fixture_root)?;
        run_cargo_rudzio(
            cargo_home,
            cargo_target_dir,
            &cargo_build_dir,
            &fixture_root,
            args,
        )
    }

    /// **Bug #10 (operator's exact shape, `-p lexer`)** — a lib member whose
    /// `[[test]]` entry is a `tests/main.rs` shim declaring `mod harness;` (and
    /// the sibling test modules), with the helper at `tests/harness/` and an
    /// `include_str!("std-numeric.me")` fixture, must aggregate exactly its four
    /// tests when filtered by package.
    #[rudzio::test]
    async fn lexer_exact_shape_package_filter_aggregates_harness(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let output = drive(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            ctx.suite_tempdir(),
            &["-p", "lexer"],
        )?;
        assert_lexer_tests(&output)
    }

    /// **Bug #10 (operator's exact shape, workspace-wide)** — the same workspace
    /// driven via `cargo rudzio test` (no package filter) must aggregate the
    /// lexer tests.
    #[rudzio::test]
    async fn lexer_exact_shape_workspace_wide_aggregates_harness(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let output = drive(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            ctx.suite_tempdir(),
            &[],
        )?;
        assert_lexer_tests(&output)
    }
}
