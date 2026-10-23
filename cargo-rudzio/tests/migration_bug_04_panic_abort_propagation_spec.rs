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
use std::process::{Command, Output};

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

/// Materialise the Bug #4 fixture under `root`: a workspace with one
/// `app` member whose single `#[rudzio::test]` panics. `profile` is the
/// raw `[profile.*]` TOML section embedded in the root manifest (empty
/// string to omit profiles entirely), which forces the chosen profile to
/// `panic = "abort"`.
fn write_fixture(root: &Path, profile: &str) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;

    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["app"]
resolver = "3"

{profile}[workspace.dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            profile = profile,
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
            panic!("boom")
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

    use super::io;
    use super::{Command, IsolatedCargoTest, Output, Path, TempDir, env, write_fixture};

    /// Spawn `cargo rudzio test` against the fixture at `root` with the
    /// isolated cargo env, forwarding `extra_args` (e.g. `--release`).
    fn run_cargo_rudzio(
        cargo_home: &Path,
        cargo_target_dir: &Path,
        root: &Path,
        extra_args: &[&str],
    ) -> io::Result<Output> {
        Command::new(
            cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
                .map_err(io::Error::other)?,
        )
        .arg("test")
        .args(extra_args)
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
    }

    #[rudzio::test]
    fn panic_abort_profile_aborts_the_runner_process(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root, "[profile.dev]\npanic = \"abort\"\n")?;

        let output = run_cargo_rudzio(ctx.cargo_home(), ctx.cargo_target_dir(), root, &[])?;

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

    #[rudzio::test]
    fn release_panic_abort_aborts_the_runner_process(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        // `--release` must forward to the aggregator build, selecting
        // `[profile.release]`, whose `panic = "abort"` then aborts the
        // runner exactly like the dev-profile case.
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root, "[profile.release]\npanic = \"abort\"\n")?;

        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            root,
            &["--release"],
        )?;

        let status = output.status;
        let aborted =
            status.signal() == Some(6_i32) || (!status.success() && status.code().is_none());
        anyhow::ensure!(
            aborted,
            "expected `--release` to abort on panic via [profile.release] panic = \"abort\", but \
             the runner exited {:?} (code={:?}, signal={:?}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status,
            status.code(),
            status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }

    #[rudzio::test]
    fn no_panic_abort_reports_failed_and_exits_one(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        // Negative control: without `panic = "abort"`, the panicking test
        // unwinds to the runner's catch layer, which reports the panic and
        // exits cleanly with code 1 — NOT a signal exit.
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root, "")?;

        let output = run_cargo_rudzio(ctx.cargo_home(), ctx.cargo_target_dir(), root, &[])?;

        let status = output.status;
        anyhow::ensure!(
            status.code() == Some(1_i32),
            "expected a clean exit 1 for an unwinding panic (no panic=abort), but got \
             {status:?} (code={:?}, signal={:?})",
            status.code(),
            status.signal(),
        );
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::ensure!(
            combined.contains("panicked"),
            "expected the runner to report the panicking test, but saw:\n{combined}",
        );
        Ok(())
    }
}
