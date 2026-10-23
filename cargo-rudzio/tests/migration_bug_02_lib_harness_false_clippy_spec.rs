//! Reproduction for migration Bug #2: `[lib] harness = false` breaks
//! `cargo clippy --all-targets`.
//!
//! # Symptom
//!
//! A crate that follows the README's "Lib unit tests (no `tests/`
//! directory)" recipe — `[lib] harness = false` plus a
//! `#[cfg(test)] #[rudzio::main] fn main() {}` entry point — fails
//! `cargo clippy --all-targets --all-features -- -D warnings` with
//! `error[E0601]: main function not found in crate`.
//!
//! # Root cause
//!
//! `harness = false` turns the lib's test target into a binary. The
//! README supplies a `#[cfg(test)]` `main`, but clippy's `--all-targets`
//! pass lints the test target in a shape where that entry point does not
//! register, so the target is treated as a `main`-less binary.
//!
//! Contrast: the rudzio root manifest opts its lib out of the test target
//! entirely with `[lib] test = false` (`README.md:102-103`), which is why
//! the workspace itself does not trip this.
//!
//! # Assertion
//!
//! Spawn `cargo clippy --all-targets --all-features -- -D warnings` in a
//! scratch crate carrying the README-documented `[lib] harness = false`
//! shape and assert exit 0. Under the current toolchain the subprocess
//! exits non-zero with E0601, so this test fails until the bug is fixed.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repository root. This spec compiles inside the `cargo-rudzio` test
/// binary, so `env!("CARGO_MANIFEST_DIR")` resolves to
/// `<repo>/cargo-rudzio` and its parent is the repo root the scratch
/// crate's rudzio `path` dep must point at.
fn repo_root() -> anyhow::Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent().ok_or_else(|| {
        anyhow::anyhow!("cargo-rudzio lives at <repo>/cargo-rudzio; its parent is the repo root")
    })?;
    Ok(root.to_path_buf())
}

/// Materialise the README-documented `[lib] harness = false` scratch
/// crate at `root`: a `Cargo.toml` with `[lib] harness = false` and a
/// rudzio `path` dev-dep, plus the `src/lib.rs` shown at
/// `README.md:531-557`.
fn write_fixture(root: &Path, rudzio_root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "lib_unit_harness_false"
version = "0.1.0"
edition = "2024"

[lib]
harness = false

[dev-dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.102"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        root.join("src").join("lib.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::*;

    #[rudzio::test]
    async fn sums_correctly() -> anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        Ok(())
    }
}

#[cfg(test)]
#[rudzio::main]
fn main() {}
"#,
    )?;
    Ok(())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{Command, IsolatedCargoTest, TempDir, env, repo_root, write_fixture};

    /// Bug #2 — `[lib] harness = false` must not break
    /// `cargo clippy --all-targets`.
    #[rudzio::test]
    fn lib_harness_false_clippy_all_targets_succeeds(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let scratch = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(scratch.path(), &repo_root()?)?;

        let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(cargo_bin)
            .arg("clippy")
            .arg("--all-targets")
            .arg("--all-features")
            .arg("--")
            .arg("-D")
            .arg("warnings")
            .current_dir(scratch.path())
            .env("CARGO_HOME", ctx.cargo_home())
            .env("CARGO_TARGET_DIR", ctx.cargo_target_dir())
            .env("CARGO_NET_OFFLINE", "true")
            .output()?;

        anyhow::ensure!(
            output.status.success(),
            "`cargo clippy --all-targets --all-features -- -D warnings` failed on a \
             `[lib] harness = false` crate (status {:?}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }
}
