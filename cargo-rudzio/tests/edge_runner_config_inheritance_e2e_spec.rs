//! Edge-case e2e test for the `RUDZIO_RUNNER_ARGS` inheritance seam.
//!
//! A parent rudzio runner serializes its resolved config into the
//! `RUDZIO_RUNNER_ARGS` env var (`Config::to_runner_args` +
//! `runner::set_runner_args_env`) so that nested `cargo rudzio test` runs
//! and fixture bins spawned by e2e specs inherit the parent's effective
//! concurrency/timeout config — most importantly
//! `--threads-parallel-hardlimit=none`, which the dogfood `just test`
//! passes to stop concurrent-heavy nested suites from parking on the
//! default 16-permit gate (the 1h+ run regression).
//!
//! # Contract under test
//!
//! A nested runner launched with `RUDZIO_RUNNER_ARGS` carrying
//! `--threads-parallel-hardlimit=none` (and no explicit hardlimit flag on
//! its own argv) must resolve `Config::parallel_hardlimit == None`. The
//! fixture test introspects the resolved config handed to it by the
//! runner, so the assertion is deterministic — it does not depend on
//! timing-sensitive parking behaviour.

#![cfg(unix)]

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, RunSummary, assert_exact_test_names, assert_fresh_binary,
    assert_summary, combined_output,
};
use rudzio::runtime::tokio::Multithread;

/// Repo root = parent of `cargo-rudzio/` (where this spec lives). Used as
/// the `path` for the fixture's `rudzio` workspace dependency.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Write a single-member (`app`) workspace whose one `#[rudzio::test]`
/// asserts the runner's resolved `parallel_hardlimit` is `None`.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["app"]
resolver = "3"

[workspace.dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
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

    fs::write(
        app.join("src").join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20use rudzio::Test as _;\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn inherits_hardlimit_none(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20_t.config().parallel_hardlimit.is_none(),\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\"expected inherited --threads-parallel-hardlimit=none, got {:?}\",\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20_t.config().parallel_hardlimit\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

/// Drive `cargo rudzio test` against the fixture at `root` with the
/// isolated cargo env, injecting `RUDZIO_RUNNER_ARGS` as the inherited
/// parent-runner config.
fn run_cargo_rudzio(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    root: &Path,
) -> anyhow::Result<Output> {
    let binary = assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;
    let mut cmd = Command::new(binary);
    let _: &mut Command = cmd
        .arg("test")
        .current_dir(root)
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
        .env(
            rudzio::RUNNER_ARGS_ENV,
            "--threads-parallel-hardlimit=none",
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().map_err(io::Error::other)?;
    Ok(output)
}

#[rudzio::suite([(
    runtime = Multithread::new,
    suite = IsolatedCargoSuite,
    test = IsolatedCargoTest,
)])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, RunSummary, assert_exact_test_names, assert_summary,
        combined_output, fs, rudzio_source_root, run_cargo_rudzio, write_fixture,
    };

    /// Repo root of the rudzio workspace under test (parent of `cargo-rudzio/`).
    fn rudzio_root() -> anyhow::Result<&'static Path> {
        rudzio_source_root()
            .ok_or_else(|| anyhow::anyhow!("cargo-rudzio/tests must live under the workspace root"))
    }

    #[rudzio::test]
    fn nested_runner_inherits_hardlimit_none(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("ws-inherit");
        fs::create_dir_all(&root)?;
        write_fixture(&root, rudzio_root()?)?;
        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &root,
        )?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "expected nested runner to inherit --threads-parallel-hardlimit=none, \
             but cargo rudzio exited {:?}.\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["inner::inherits_hardlimit_none"])?;
        Ok(())
    }
}
