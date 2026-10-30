//! Behavioral spec: a downstream consumer crate that depends on BOTH the
//! `compio` facade (the `0.19.x` line) AND `rudzio` (with `runtime-compio`)
//! must link exactly ONE copy of `compio-runtime` / `compio-driver`, and a
//! `#[rudzio::test]` body must observe itself running inside that runtime.
//!
//! # Symptom (pre-fix)
//!
//! rudzio pinned `compio-runtime ^0.11.0` + `compio-driver ^0.11.4`, while
//! `compio 0.19.2` pulls `compio-runtime ^0.12.6` + `compio-driver ^0.12.5`.
//! Cargo cannot unify `0.11` and `0.12` (distinct semver minors, both pre-1.0),
//! so a consumer declaring both links **two** copies of `compio-runtime` and
//! `compio-driver`. `compio-runtime`'s "current runtime" is a
//! `scoped_thread_local!` static *inside the crate*; two copies ⇒ two
//! independent statics. rudzio's `block_on` enters copy A's TLS, while a test
//! body compiled against copy B (the `compio` facade) asking
//! `Runtime::try_current()` sees `is_set() == false` → `None`, and
//! `Runtime::current()` → `panic!("not in a compio runtime")`.
//!
//! # Fix
//!
//! Bump rudzio's `compio-runtime` / `compio-driver` to the `0.12` line so the
//! consumer's `compio 0.19.2` and rudzio unify onto a single copy. This spec
//! asserts the unified graph and the runtime-context observation, so any
//! regression that reintroduces a `0.11`/`0.12` split fails loudly.
//!
//! # Offline / registry note
//!
//! The fixture declares `compio = "0.19.2"` (the facade), which is NOT part of
//! the `IsolatedCargoSuite` pre-warmed registry (that only covers `anyhow`,
//! `tokio`, and rudzio's own transitive graph). The facade (and, in the RED
//! case, the stale `compio-runtime 0.11` / `compio-driver 0.11` copies) must be
//! fetched, so every subprocess below pins `CARGO_NET_OFFLINE=false` explicitly
//! — this also defeats the sibling-spec race where a process-wide
//! `CARGO_NET_OFFLINE=true` leaks into our `cargo`/`cargo-rudzio` spawns.
#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// The consumer's `src/lib.rs`: a `#[rudzio::suite]` under the compio runtime
/// whose single test asserts it is genuinely executing inside the (single)
/// compio runtime — `compio::runtime::Runtime::try_current()` returns `Some`
/// only when the `compio` facade and rudzio resolved to the same
/// `compio-runtime` copy.
const CONSUMER_LIB_RS: &str = r#"use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::compio::Runtime as CompioRuntime;

#[rudzio::suite([
    (runtime = CompioRuntime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn runs_inside_single_compio_runtime(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(
            compio::runtime::Runtime::try_current().is_some(),
            "`compio` facade and rudzio resolved to different compio-runtime copies: \
             the test body is not executing inside the (single) compio runtime"
        );
        Ok(())
    }
}
"#;

/// Repo root = parent of `cargo-rudzio/`. The fixture's `rudzio` dependency is
/// a path dep pointing here, so it builds against the code under test.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Materialise a single-member workspace whose `consumer` lib depends on the
/// `compio` facade (`0.19.2`) in `[dependencies]` and on `rudzio`
/// (`runtime-compio` + `common`) in `[dev-dependencies]`.
fn write_fixture(root: &Path) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;

    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["consumer"]
resolver = "3"

[workspace.dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-compio", "common"] }}
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;

    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join("src"))?;
    fs::write(
        consumer.join("Cargo.toml"),
        r#"[package]
name = "consumer"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
name = "consumer"
path = "src/lib.rs"

[dependencies]
compio = "0.19.2"
rudzio = { workspace = true }

[dev-dependencies]
anyhow = "=1.0.104"
"#,
    )?;
    fs::write(consumer.join("src").join("lib.rs"), CONSUMER_LIB_RS)?;
    Ok(())
}

/// Spawn `cargo tree -d -e normal` against `root` with the isolated cargo env.
///
/// `-d` restricts output to *duplicated* packages, so an empty `compio-runtime`
/// / `compio-driver` result set proves a unified graph. `CARGO_NET_OFFLINE=false`
/// lets the `compio` facade resolve (see module doc).
fn run_cargo_tree_d(root: &Path, cargo_home: &Path, cargo_target_dir: &Path) -> io::Result<Output> {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["tree", "-d", "-e", "normal"])
        .current_dir(root)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "false")
        .env("NO_COLOR", "1")
        .output()
}

/// Spawn `cargo rudzio test` against `root` with the isolated cargo env.
fn run_cargo_rudzio(root: &Path, cargo_home: &Path, cargo_target_dir: &Path) -> io::Result<Output> {
    Command::new(
        cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .map_err(io::Error::other)?,
    )
    .arg("test")
    .current_dir(root)
    .env(
        "CARGO",
        env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
    )
    .env("CARGO_HOME", cargo_home)
    .env("CARGO_TARGET_DIR", cargo_target_dir)
    .env("CARGO_NET_OFFLINE", "false")
    .env("NO_COLOR", "1")
    .output()
}

#[rudzio::suite([(
    runtime = TokioMultithread::new,
    suite = IsolatedCargoSuite,
    test = IsolatedCargoTest
)])]
mod tests {
    use super::{IsolatedCargoTest, TempDir, run_cargo_rudzio, run_cargo_tree_d, write_fixture};
    use cargo_rudzio_test_helpers::{
        RunSummary, assert_exact_test_names, assert_summary, combined_output,
    };

    /// `cargo tree -d -e normal` must report no duplicate `compio-runtime` /
    /// `compio-driver` (a unified graph prints neither name under `-d`).
    #[rudzio::test]
    fn cargo_tree_reports_no_duplicate_compio_crates(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root = workspace.path();
        write_fixture(root)?;

        let output = run_cargo_tree_d(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "`cargo tree -d -e normal` must succeed:\n{text}"
        );
        // `cargo tree` writes the dependency tree to stdout and download/progress
        // chatter to stderr. `-d` restricts stdout to *duplicated* packages, each
        // printed as a top-level `name vX.Y.Z` header at column 0; non-duplicate
        // nodes appear only as indented children. So a `compio-runtime` /
        // `compio-driver` header line at column 0 is exactly a duplicate.
        let tree = String::from_utf8_lossy(&output.stdout);
        let compio_duplicated = tree.lines().any(|line| {
            let header = line.trim_start();
            header.starts_with("compio-runtime") || header.starts_with("compio-driver")
        });
        anyhow::ensure!(
            !compio_duplicated,
            "downstream `compio 0.19.2` + rudzio must unify on ONE compio-runtime / \
             compio-driver copy, but `cargo tree -d -e normal` reports a duplicate:\n\
             --- stdout (tree) ---\n{tree}",
        );
        Ok(())
    }

    /// The consumer's compio-context test must pass under a single runtime: one
    /// test executed, zero failed, and its exact name observed.
    #[rudzio::test]
    fn compio_context_test_runs_under_single_compio_runtime(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root = workspace.path();
        write_fixture(root)?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "`cargo rudzio test` must succeed on the downstream fixture:\n{text}"
        );
        assert_summary(
            &text,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&text, &["runs_inside_single_compio_runtime"])?;
        Ok(())
    }
}
