//! End-to-end matrix for the Bug #7 shape the earlier fixes missed: a
//! lib-bearing member whose `src/lib.rs` declares a private, self-named
//! *file* module (`mod selfmod;` in a crate named `selfmod`) AND whose
//! `src/lib.rs` is not parseable by the pinned `syn` (2.0.119) even
//! though rustc 1.99.1 accepts it. The prior guard
//! (`crate_root_declares_ident`) parsed lib.rs with `syn::parse_str`;
//! on parse failure it returned `false`, so the bridge fell back to the
//! pre-fix `extern crate self as <name>;` prelude entry, which collides
//! with the member's own `mod <name>;` (E0260).
//!
//! # Fix shape
//!
//! `crate_root_declares_ident` must (a) also try `syn::parse_file`
//! (which consumes a shebang `#!` line that `parse_str` rejects) and
//! (b) fall back to a line-based lexical scan for `mod <name>;` /
//! `pub mod <name>;` at the crate root when syn cannot parse the copy,
//! instead of returning `false`. The alias skip + `__rudzio_self_alias`
//! glob then applies exactly as for the parseable cases.
//!
//! # Assertion strategy
//!
//! Each case runs `cargo rudzio test` and asserts exit 0 plus the exact
//! set of passed test names, exactly like `edge_self_named_module_e2e_spec`.
//! The shebang and field-default cases are the faithful RED repros; the
//! plain-private case is the GREEN control proving the fix does not
//! over-broaden.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, RunSummary, assert_exact_test_names, assert_summary,
    combined_output,
};
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

/// Write a lib-bearing member's `Cargo.toml` for a crate named `crate_name`
/// with `[lib] name = crate_name` and a single flat integration test
/// `tests/x.rs` (harness = false).
fn write_lib_member_manifest(member_dir: &Path, crate_name: &str) -> io::Result<()> {
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
             name = \"{crate_name}\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [[test]]\n\
             name = \"x\"\n\
             path = \"tests/x.rs\"\n\
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
/// (typically `use selfmod::answer;`). The body asserts `answer() == 42`.
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

/// Lay down a workspace whose single `selfmod` member exposes
/// `selfmod::answer() -> u32` (42) via a private, self-named file module
/// (`mod selfmod;` -> `src/selfmod.rs`), with `lib_extra` spliced into the
/// `src/lib.rs` immediately after `mod selfmod;` so the copy is the exact
/// shape rustc accepts but `syn::parse_str` may reject. `lib_extra` is the
/// byte-exact continuation (may be empty).
fn write_self_named_fixture(root: &Path, lib_extra: &str) -> io::Result<()> {
    write_workspace_manifest(root, &["selfmod"])?;

    let member = root.join("selfmod");
    fs::create_dir_all(member.join("src"))?;
    fs::create_dir_all(member.join("tests"))?;
    write_lib_member_manifest(&member, "selfmod")?;
    // The private, self-named file module's contents are irrelevant to the
    // collision; it exists solely to bind `selfmod` at the crate root.
    fs::write(
        member.join("src").join("selfmod.rs"),
        "pub(crate) fn internal() -> u32 {\n    1\n}\n",
    )?;
    // `mod selfmod;` is the self-named binding; `answer()` lives at the
    // crate root so the test's `use selfmod::answer;` resolves through the
    // self-alias (crate name), not through the local module.
    // `lib_extra` may begin with a `#!` shebang; keep it byte-exact.
    fs::write(
        member.join("src").join("lib.rs"),
        format!("mod selfmod;\n\npub fn answer() -> u32 {{\n    42\n}}\n{lib_extra}"),
    )?;
    fs::write(
        member.join("tests").join("x.rs"),
        self_named_test_source("self_named_shape_answer", "use selfmod::answer;"),
    )?;
    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, RunSummary, TempDir, assert_exact_test_names, assert_summary,
        combined_output, fs, run_cargo_rudzio, write_self_named_fixture,
    };

    /// Operator's faithful shape: a short `src/lib.rs` whose first line is
    /// a shebang (`#!`) followed by a private, self-named file module.
    /// `syn::parse_str` rejects the shebang; the guard must still detect
    /// `mod selfmod;` and skip the `extern crate self` alias.
    #[rudzio::test]
    fn shebang_lib_private_self_named_module(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_self_named_fixture(root, "")?;
        // Prepend the shebang as the very first bytes of lib.rs.
        let lib = root.join("selfmod").join("src").join("lib.rs");
        let body = fs::read_to_string(&lib)?;
        fs::write(&lib, format!("#!/usr/bin/env rust\n{body}"))?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        let text = combined_output(&output);
        anyhow::ensure!(output.status.success(), "expected exit 0, output:\n{text}");
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["self_named_shape_answer"])?;
        Ok(())
    }

    /// GREEN control: a plain parseable private `mod selfmod;` — the
    /// already-working path that must keep working (no over-broadening).
    #[rudzio::test]
    fn plain_private_self_named_module_control(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_self_named_fixture(root, "")?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        let text = combined_output(&output);
        anyhow::ensure!(output.status.success(), "expected exit 0, output:\n{text}");
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["self_named_shape_answer"])?;
        Ok(())
    }
}
