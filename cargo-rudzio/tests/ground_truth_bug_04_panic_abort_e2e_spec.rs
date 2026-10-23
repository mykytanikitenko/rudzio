//! Ground-truth e2e reproducer for Bug #4: `[profile.dev] panic = "abort"`
//! at the consumer workspace root is re-emitted into the aggregator
//! `Cargo.toml`, but the panicking test still unwinds to the runner's
//! catch layer and exits cleanly instead of aborting.
//!
//! # Why (root cause, established by source trace)
//!
//! `build_cargo_toml` (generate.rs) DOES copy the `[profile]` table into the
//! generated aggregator manifest — the aggregator's own `Cargo.toml` shows
//! `[profile.dev]\npanic = "abort"`. But every bridge crate is emitted with
//! its own empty `[workspace]` stanza, making it an independent workspace
//! root that does NOT inherit the aggregator's `[profile]`. The panicking
//! `#[rudzio::test]` body is compiled into the bridge crate
//! (`app_rudzio_bridge`), so it compiles with the default `panic = "unwind"`
//! and the panic unwinds to the runner's `catch_unwind` instead of aborting.
//!
//! # Symptom
//!
//! ```text
//! [PANIC]   aborting_panic   <tokio::Multithread, ...>
//! test result: FAILED. 0 passed; ... 1 panicked; ...
//! ```
//! The runner exits with a clean `Some(1)` code; no SIGABRT.
//!
//! # Assertion (flips GREEN once fixed)
//!
//! With `panic = "abort"` propagated into the bridge compile unit, the
//! panicking test must abort the whole runner process: SIGABRT (signal 6),
//! no clean exit code.
//!
//! # Fresh build (no stale binary)
//!
//! Drives `cargo run -p cargo-rudzio -- test` (not
//! `env!("CARGO_BIN_EXE_cargo-rudzio")`) so the cargo-rudzio binary is
//! rebuilt from source. `RUSTC_WRAPPER=` + `SCCACHE_DISABLE=1` + task-local
//! `CARGO_TARGET_DIR` / `CARGO_BUILD_BUILD_DIR` defeat sccache / shared-target
//! staleness.

#![cfg(unix)]

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// Repo root = parent of `cargo-rudzio/` (the rudzio workspace).
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
}

/// Resolve the ambient cargo home that already holds cargo-rudzio's own
/// dependency graph (`cargo_metadata`, `toml_edit`, rudzio-migrate, …). The
/// per-test hardlinked home only pre-warms anyhow/tokio/rudzio, so it cannot
/// build cargo-rudzio itself.
fn cargo_home_with_full_graph() -> PathBuf {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// Drive `cargo run -p cargo-rudzio -- test <args>` from the repo root with a
/// FRESH build into task-local target/build dirs.
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

/// Materialise the Bug #4 fixture: a workspace whose root manifest sets
/// `[profile.dev] panic = "abort"` and one `app` member whose single
/// `#[rudzio::test]` panics.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    let app = root.join("app");
    fs::create_dir_all(app.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"app\"]\n\
             resolver = \"3\"\n\
             \n\
             [profile.dev]\n\
             panic = \"abort\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        app.join("Cargo.toml"),
        "[package]\n\
         name = \"app\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         \n\
         [lib]\n\
         name = \"app\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;
    fs::write(
        app.join("src").join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn aborting_panic(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20panic!(\"boom\")\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::{
        IsolatedCargoTest, cargo_home_with_full_graph, repo_root, run_cargo_rudzio_fresh,
        write_fixture,
    };

    /// **Bug #4** — `panic = "abort"` must abort the runner on a panicking
    /// test, not unwind to the catch layer.
    #[rudzio::test]
    async fn panic_abort_aborts_the_runner(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("panic-abort-ws");
        write_fixture(&workspace, repo_root())?;

        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        let manifest = workspace.join("Cargo.toml").to_string_lossy().into_owned();
        let output = run_cargo_rudzio_fresh(
            &cargo_home_with_full_graph(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &["--manifest-path", &manifest],
        )?;

        let status = output.status;
        // Correct: SIGABRT (signal 6) or a signal-terminated exit (no clean
        // code). Bug #4: the bridge compiles with unwind (own workspace
        // root, no [profile] inheritance), so the runner exits cleanly 1.
        let aborted =
            status.signal() == Some(6_i32) || (!status.success() && status.code().is_none());
        anyhow::ensure!(
            aborted,
            "expected the runner to abort on panic (SIGABRT / signal exit) under \
             panic = \"abort\", but it exited {status:?} (code={:?}, signal={:?}). Bug #4: \
             the bridge crate is its own workspace root and does not inherit the \
             aggregator [profile.dev] panic = \"abort\".\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status.code(),
            status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }
}
