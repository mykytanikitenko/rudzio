//! End-to-end reproducer for Bug #10: `cargo rudzio test -p lexer`
//! fails for a lib crate whose flat integration test references a
//! shared test-helper submodule and a non-Rust fixture file.
//!
//! Two distinct gaps in the proxy emission:
//!
//! 1. A `mod harness;` declaration in a flat test root
//!    (`tests/integration.rs`) resolves — under stock cargo — to the
//!    sibling `tests/harness/mod.rs` (a test crate root resolves its
//!    `mod X;` children in its own directory, the same rule behind the
//!    canonical `tests/common/mod.rs` pattern). The proxy emitter's
//!    helper discovery looked under `tests/integration/` instead, so it
//!    never found the helper, never hoisted it to the bridge crate
//!    root, and left the stripped-out `mod harness;` dangling — the
//!    bridge compile fails with `cannot find 'harness' in the crate
//!    root`.
//!
//! 2. `include_str!("fixtures/std-numeric.me")` (and `include_bytes!`)
//!    in a proxied test file resolves relative to the *proxied* file's
//!    location, but the emitter only copied `.rs` files into the proxy
//!    dir — the `.me` fixture was dropped, so the bridge compile fails
//!    with `couldn't read .../std-numeric.me`.
//!
//! Post-fix the helper is hoisted to the bridge crate root (with the
//! `crate::harness::…` path rewritten to the synthesised module name)
//! and the non-Rust fixture is copied verbatim beside the proxy, so the
//! runner reports both tests passed.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/` (where this spec lives). Used
/// as the `path` for the fixture's `rudzio` workspace dependency so the
/// synthetic workspace builds against the rudzio code under test rather
/// than a published version.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Materialise the Bug #10 fixture under `root`: a workspace whose
/// single `lexer` lib member declares `src/lib.rs` plus a flat
/// `tests/integration.rs` that (a) `mod harness;`-imports a shared
/// helper submodule at `tests/harness/mod.rs` and (b) reads a non-Rust
/// fixture via `include_str!("fixtures/std-numeric.me")`.
fn write_fixture(root: &Path) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;

    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"lexer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )?;

    let lexer = root.join("lexer");
    fs::create_dir_all(lexer.join("src"))?;
    // `tests/harness/` is a directory (NOT `tests/harness.rs`): a flat
    // `tests/harness.rs` would itself be auto-discovered as a test
    // binary, which is exactly the collision the shared-helper pattern
    // exists to avoid. `mod harness;` in the flat test root resolves to
    // `tests/harness/mod.rs`.
    fs::create_dir_all(lexer.join("tests").join("harness"))?;
    fs::create_dir_all(lexer.join("tests").join("fixtures"))?;
    fs::write(
        lexer.join("Cargo.toml"),
        r#"[package]
name = "lexer"
version = "0.1.0"
edition = "2024"
publish = false
autotests = false

[lib]
name = "lexer"
path = "src/lib.rs"

[[test]]
name = "integration"
path = "tests/integration.rs"
harness = false

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
"#,
    )?;

    // The lib body: a real member lib target so the bridge mounts its
    // integration-test proxy into a lib-bearing bridge.
    fs::write(
        lexer.join("src").join("lib.rs"),
        "pub fn lex(input: &str) -> usize {\n    input.len()\n}\n",
    )?;

    // Shared test helper. `mod harness;` + `use crate::harness::helper;`
    // is the exact shape the proxy emitter must hoist + rewrite.
    fs::write(
        lexer.join("tests").join("harness").join("mod.rs"),
        "pub fn helper() -> i32 {\n    42\n}\n",
    )?;

    // Non-Rust fixture read via `include_str!`. The `.me` extension
    // mirrors the real `lexer` crate's `std-numeric.me`.
    fs::write(
        lexer.join("tests").join("fixtures").join("std-numeric.me"),
        "42.0",
    )?;

    fs::write(
        lexer.join("tests").join("integration.rs"),
        r#"// rudzio_test marker
mod harness;
use crate::harness::helper;

const NUMERIC: &str = include_str!("fixtures/std-numeric.me");

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
mod tests {
    use super::NUMERIC;
    use super::helper;

    #[rudzio::test]
    fn harness_helper_resolves() -> anyhow::Result<()> {
        anyhow::ensure!(helper() == 42, "expected helper() == 42");
        Ok(())
    }

    #[rudzio::test]
    fn fixture_include_str_resolves() -> anyhow::Result<()> {
        anyhow::ensure!(
            NUMERIC == "42.0",
            "expected include_str fixture content \"42.0\", got {NUMERIC:?}"
        );
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
"#,
    )?;

    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{Command, IsolatedCargoTest, Path, TempDir, env, write_fixture};

    #[rudzio::test]
    fn lexer_member_aggregates_harness_and_fixture(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root)?;

        let output = Command::new(cargo_rudzio_test_helpers::assert_fresh_binary(env!(
            "CARGO_BIN_EXE_cargo-rudzio"
        ))?)
        .arg("test")
        .arg("-p")
        .arg("lexer")
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", ctx.cargo_home())
        .env("CARGO_TARGET_DIR", ctx.cargo_target_dir())
        .env("CARGO_NET_OFFLINE", "true")
        .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test -p lexer must exit 0 when the member's flat integration \
             test mounts a shared `tests/harness/` submodule and reads a non-Rust fixture; \
             exit = {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code()
        );
        anyhow::ensure!(
            stdout.contains("harness_helper_resolves")
                && stdout.contains("fixture_include_str_resolves"),
            "runner output must report both the helper test and the fixture test as passed; \
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );

        Ok(())
    }
}
