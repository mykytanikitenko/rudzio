//! Precedence spec for the build-time identity stamp.
//!
//! `cargo-rudzio/build.rs` is compiled standalone and executed with a
//! controlled environment, so the code under test is the shipped build
//! script rather than a copy of its logic. The stamp precedence is: a
//! non-empty `CARGO_RUDZIO_BUILD_GIT_HEAD` override, then `git` scoped to
//! the manifest dir, then `unknown`. The version always comes from the
//! `CARGO_PKG_VERSION` the script sees at runtime, which is how a bridge
//! hands it the real member's version.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// `rustc-env` name for the resolved git HEAD.
const HEAD_KEY: &str = "CARGO_RUDZIO_BUILD_GIT_HEAD";
/// `rustc-env` name for the resolved working-tree dirty state.
const DIRTY_KEY: &str = "CARGO_RUDZIO_BUILD_GIT_DIRTY";
/// `rustc-env` name for the resolved crate version.
const VERSION_KEY: &str = "CARGO_RUDZIO_BUILD_VERSION";

/// Compile the real `build.rs` into `dir` and return the executable path.
fn compile_build_script(dir: &Path) -> anyhow::Result<PathBuf> {
    let source = repo_root().join("cargo-rudzio").join("build.rs");
    let exe = dir.join("stamp_build_script");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .args(["--edition", "2021", "--crate-name", "stamp_build_script"])
        .arg(&source)
        .arg("-o")
        .arg(&exe)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "compiling {} failed: {}",
        source.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(exe)
}

/// Run the compiled build script with a controlled environment and return
/// its identity `rustc-env` entries (head, dirty, version) keyed by name.
fn run_script(
    exe: &Path,
    manifest_dir: &Path,
    version: &str,
    overrides: &[(&str, &str)],
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut command = Command::new(exe);
    let _env_clear: &mut Command = command.env_clear();
    let _path: &mut Command = command.env("PATH", env::var_os("PATH").unwrap_or_default());
    let _manifest_dir: &mut Command = command.env("CARGO_MANIFEST_DIR", manifest_dir);
    let _pkg_version: &mut Command = command.env("CARGO_PKG_VERSION", version);
    let _profile: &mut Command = command.env("PROFILE", "debug");
    for (key, value) in overrides {
        let _env: &mut Command = command.env(key, value);
    }
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "build script exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let mut entries = BTreeMap::new();
    for line in stdout.lines() {
        let Some(pair) = line.strip_prefix("cargo::rustc-env=") else {
            continue;
        };
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if [HEAD_KEY, DIRTY_KEY, VERSION_KEY].contains(&key) {
            drop(entries.insert(key.to_owned(), value.to_owned()));
        }
    }
    Ok(entries)
}

/// The expected identity map for `(head, dirty, version)`.
fn expected(head: &str, dirty: &str, version: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (HEAD_KEY.to_owned(), head.to_owned()),
        (DIRTY_KEY.to_owned(), dirty.to_owned()),
        (VERSION_KEY.to_owned(), version.to_owned()),
    ])
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use std::fs;

    use super::{
        IsolatedCargoTest, compile_build_script, current_git_head, expected, repo_root, run_script,
    };

    /// A sentinel SHA that is definitely not the checkout's real HEAD.
    const OVERRIDE_SHA: &str = "feedfacefeedfacefeedfacefeedfacefeedface";

    #[rudzio::test]
    fn env_override_wins_over_git(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let exe = compile_build_script(ctx.suite_tempdir())?;
        let got = run_script(
            &exe,
            &repo_root().join("cargo-rudzio"),
            "7.8.9",
            &[
                ("CARGO_RUDZIO_BUILD_GIT_HEAD", OVERRIDE_SHA),
                ("CARGO_RUDZIO_BUILD_GIT_DIRTY", "clean"),
            ],
        )?;
        anyhow::ensure!(
            got == expected(OVERRIDE_SHA, "clean", "7.8.9"),
            "unexpected stamp: {got:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn git_is_used_without_override(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let head = current_git_head()
            .ok_or_else(|| anyhow::anyhow!("checkout has no resolvable git HEAD"))?;
        let exe = compile_build_script(ctx.suite_tempdir())?;
        let got = run_script(
            &exe,
            &repo_root().join("cargo-rudzio"),
            "0.2.0",
            &[("CARGO_RUDZIO_BUILD_GIT_DIRTY", "clean")],
        )?;
        anyhow::ensure!(
            got == expected(&head, "clean", "0.2.0"),
            "unexpected stamp: {got:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn empty_override_falls_through_to_git(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let head = current_git_head()
            .ok_or_else(|| anyhow::anyhow!("checkout has no resolvable git HEAD"))?;
        let exe = compile_build_script(ctx.suite_tempdir())?;
        let got = run_script(
            &exe,
            &repo_root().join("cargo-rudzio"),
            "0.2.0",
            &[
                ("CARGO_RUDZIO_BUILD_GIT_HEAD", "  "),
                ("CARGO_RUDZIO_BUILD_GIT_DIRTY", "clean"),
            ],
        )?;
        anyhow::ensure!(
            got == expected(&head, "clean", "0.2.0"),
            "unexpected stamp: {got:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn unknown_without_override_or_git(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let exe = compile_build_script(ctx.suite_tempdir())?;
        let no_git = ctx.suite_tempdir().join("no-git-source");
        fs::create_dir_all(&no_git)?;
        let got = run_script(&exe, &no_git, "0.2.0", &[])?;
        anyhow::ensure!(
            got == expected("unknown", "unknown", "0.2.0"),
            "unexpected stamp: {got:?}"
        );
        Ok(())
    }
}
