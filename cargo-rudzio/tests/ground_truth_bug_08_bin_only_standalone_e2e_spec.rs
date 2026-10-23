//! Ground-truth e2e reproducer for Bug #8: a STANDALONE bin-only crate
//! (its own workspace root, not a workspace member) never aggregates its
//! `tests/*.rs` integration tests into the runner.
//!
//! # Why standalone (not a workspace member)
//!
//! The existing `edge_bin_only_e2e_spec` drives a bin-only crate as a
//! workspace *member* (`manifest_dir != workspace_root`) and passes. The
//! operator's repro points `cargo rudzio test --manifest-path <crate>/Cargo.toml`
//! at a single standalone crate, where the crate IS its own workspace root.
//! In that shape `build_main_rs` and the aggregator's `[dependencies]`
//! emission both skip the member via the `paths_equal(manifest_dir,
//! workspace_root)` guard, so the bridge rlib is never `extern crate`d into
//! the runner and linkme's `TEST_TOKENS` slice is empty.
//!
//! # Symptom
//!
//! ```text
//! thread 'main' panicked at .../linkme-0.3.37/src/distributed_slice.rs:246:18:
//! unsafe precondition(s) violated: slice::from_raw_parts requires the
//! pointer to be aligned and non-null ...
//! ```
//! Process aborts with SIGABRT (exit 134) instead of running the test.
//!
//! # Assertion (flips GREEN once fixed)
//!
//! The runner must exit 0 and report the standalone bin-only crate's
//! integration test as passed.
//!
//! # Fresh build (no stale binary)
//!
//! This spec drives `cargo run -p cargo-rudzio -- test` (not
//! `env!("CARGO_BIN_EXE_cargo-rudzio")`), forcing cargo to rebuild the
//! cargo-rudzio binary from source before running it. `RUSTC_WRAPPER=` +
//! `SCCACHE_DISABLE=1` + a task-local `CARGO_TARGET_DIR` /
//! `CARGO_BUILD_BUILD_DIR` eliminate any shared-target or sccache
//! contamination that previously made a stale binary pass as "fixed".

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// Repo root = parent of `cargo-rudzio/` (the rudzio workspace).
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cargo-rudzio/tests must live under the repo root")
}

/// Resolve the cargo home that already holds cargo-rudzio's own dependency
/// set (toml_edit, cargo_metadata, clap, …). The per-test hardlinked home
/// (`IsolatedCargoTest::cargo_home`) only pre-warms anyhow/tokio/rudzio, so
/// it cannot build cargo-rudzio itself. `cargo run -p cargo-rudzio` must run
/// against a home that has the full graph.
fn cargo_home_with_full_graph() -> PathBuf {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// Drive `cargo run -p cargo-rudzio -- test <args>` from the repo root with a
/// FRESH build: empty `RUSTC_WRAPPER`, `SCCACHE_DISABLE=1`, and task-local
/// `CARGO_TARGET_DIR` / `CARGO_BUILD_BUILD_DIR`. `cargo run` rebuilds the
/// cargo-rudzio binary from source, so a stale shared-target binary can never
/// satisfy the assertion.
fn run_cargo_rudzio_fresh(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    args: &[&str],
) -> io::Result<Output> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Command::new(cargo_bin)
        .arg("run")
        .arg("-p")
        .arg("cargo-rudzio")
        .arg("--")
        .arg("test")
        .args(args)
        .current_dir(repo_root())
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .env("CARGO_NET_OFFLINE", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Materialise a STANDALONE bin-only crate at `crate_dir`: `src/main.rs`
/// (`fn main() {}`), a `tests/x.rs` with one `#[rudzio::test]`, and a
/// `Cargo.toml` (edition 2024) whose dev-dep on `rudzio` uses the
/// `runtime-tokio-multi-thread` + `common` features. No `[workspace]` table,
/// so the crate is its own workspace root — the shape that trips the
/// `paths_equal(manifest_dir, workspace_root)` skip.
fn write_standalone_bin_only_crate(crate_dir: &Path, rudzio_root: &Path) -> io::Result<()> {
    let src = crate_dir.join("src");
    let tests = crate_dir.join("tests");
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(&tests)?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"mybin\"\n\
             version = \"0.1.0\"\n\
             edition = \"2024\"\n\
             publish = false\n\
             \n\
             [[bin]]\n\
             name = \"mybin\"\n\
             path = \"src/main.rs\"\n\
             \n\
             [dev-dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    std::fs::write(src.join("main.rs"), "fn main() {}\n")?;
    std::fs::write(
        tests.join("x.rs"),
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn bin_only_t(_c: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;
    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        IsolatedCargoTest, cargo_home_with_full_graph, repo_root, run_cargo_rudzio_fresh,
        write_standalone_bin_only_crate,
    };

    /// **Bug #8** — a standalone bin-only crate must aggregate its
    /// `tests/x.rs` integration test into the runner.
    #[rudzio::test]
    async fn standalone_bin_only_crate_aggregates_integration_test(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let crate_dir = ctx.suite_tempdir().join("standalone-mybin");
        write_standalone_bin_only_crate(&crate_dir, repo_root())?;

        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .expect("cargo target dir has a parent")
            .join("cargo-build-dir");

        let manifest = crate_dir
            .join("Cargo.toml")
            .to_string_lossy()
            .into_owned();
        let output = run_cargo_rudzio_fresh(
            &cargo_home_with_full_graph(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &["--manifest-path", &manifest, "-p", "mybin"],
        )?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::ensure!(
            output.status.success(),
            "standalone bin-only crate must run its integration test (exit 0), but the runner \
             exited {:?}. Bug #8: the bridge rlib is never linked, so TEST_TOKENS is empty and \
             linkme aborts.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code(),
        );
        anyhow::ensure!(
            stdout.contains("bin_only_t"),
            "runner stdout must name the aggregated test `bin_only_t`.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
        Ok(())
    }
}
