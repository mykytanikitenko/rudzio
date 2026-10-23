//! Reproducer for Bug #4: `panic = "abort"` is not propagated to the
//! auto-runner.
//!
//! # Symptom (pre-fix, cargo-rudzio 0.1.2)
//!
//! A workspace whose root `Cargo.toml` sets `[profile.dev] panic = "abort"`
//! still reports "test FAILED (panicked), exit 1" for a panicking test
//! instead of aborting with SIGABRT. The auto-runner is a separate
//! generated project that does not inherit the workspace `[profile]`, so
//! `panic = "abort"` never applies to the compiled test binary and the
//! panic unwinds to the runner's catch layer instead of aborting.
//!
//! # Reproduction
//!
//! Fixture: a workspace with one `app` member. The root manifest pins
//! `panic = "abort"` under `[profile.dev]`. `app`'s `src/lib.rs` holds a
//! single `#[rudzio::test]` whose body calls `panic!("boom")`. Invoke
//! `cargo-rudzio test` as a subprocess from the workspace root with the
//! isolated cargo env vars.
//!
//! Correct behavior: the panicking test aborts the whole runner process
//! (SIGABRT / signal 6, no clean exit code). Pre-fix, the profile is not
//! inherited, so the panic unwinds, the runner catches it, and exits with
//! a clean `Some(1)` code — the assertion below must FAIL.
//!
//! Unix-only: SIGABRT / signal-based exit statuses are a Unix process
//! model; Windows is out of scope for this repro.
#![cfg(unix)]

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
#[expect(
    clippy::expect_used,
    reason = "CARGO_MANIFEST_DIR is always an absolute path whose parent is the workspace root"
)]
fn rudzio_source_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cargo-rudzio/tests must live under the workspace root")
}

/// Materialise the Bug #4 fixture under `root`: a workspace whose
/// `[profile.dev] panic = "abort"` should force every test binary to
/// abort on panic, plus an `app` member whose single `#[rudzio::test]`
/// panics.
fn write_fixture(root: &Path) -> io::Result<()> {
    let rudzio_root = rudzio_source_root();

    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["app"]
resolver = "3"

[profile.dev]
panic = "abort"

[workspace.dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.102"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;

    let app = root.join("app");
    fs::create_dir_all(app.join("src"))?;
    fs::write(
        app.join("Cargo.toml"),
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
name = "app"
path = "src/lib.rs"

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
"#,
    )?;

    // The `#[rudzio::test]` must live in `src/lib.rs` (the lib target),
    // not a `[[test]]`/`[[bin]]` target: the bridge compiles the lib
    // source and links it into the aggregator, so only lib-compile-unit
    // `linkme` registrations reach the runner — tests in `tests/` and
    // `src/bin/` are invisible (Bug #16). The `rudzio_test` marker is
    // what `detect_src_rudzio_suite` greps for in `src/**`.
    fs::write(
        app.join("src").join("lib.rs"),
        r#"// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn aborting_panic(_t: &super::Test) -> anyhow::Result<()> {
            panic!("boom");
            #[allow(unreachable_code)]
            Ok(())
        }
    }
}
"#,
    )?;

    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::{Command, IsolatedCargoTest, Path, TempDir, env, write_fixture};

    #[rudzio::test]
    fn panic_abort_profile_aborts_the_runner_process(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root)?;

        let output = Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .arg("test")
            .current_dir(root)
            .env(
                "CARGO",
                env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
            )
            .env("CARGO_HOME", ctx.cargo_home())
            .env("CARGO_TARGET_DIR", ctx.cargo_target_dir())
            .env("CARGO_NET_OFFLINE", "true")
            .output()?;

        let status = output.status;
        // With `panic = "abort"` the panicking test must abort the whole
        // runner process: SIGABRT (signal 6), no clean exit code. Pre-fix
        // the profile is NOT inherited, so the panic unwinds, the runner
        // catches it, and exits cleanly with code Some(1).
        let aborted =
            status.signal() == Some(6_i32) || (!status.success() && status.code().is_none());
        anyhow::ensure!(
            aborted,
            "expected the runner to abort on panic (SIGABRT / signal exit), but it exited \
             {:?} (code={:?}, signal={:?}). `panic = \"abort\"` was NOT propagated to the \
             auto-runner.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status,
            status.code(),
            status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }
}
