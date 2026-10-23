//! Freshness-guard spec: `assert_fresh_binary` rejects a stale or foreign
//! `cargo-rudzio` binary and accepts the freshly-built one.
//!
//! # Why
//!
//! `env!("CARGO_BIN_EXE_cargo-rudzio")` is a compile-time path into the
//! ambient `CARGO_TARGET_DIR`. A shared target dir can hand the suite a
//! binary built from a different HEAD than the test source, so a fix
//! reports green against a binary that never carried it. Every spawn in
//! this suite routes through `assert_fresh_binary`, which proves the
//! binary reports the live checkout HEAD before the suite exercises it.
//! These tests are the negative and positive proof that the guard fires
//! on the wrong binary and accepts the right one.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest, assert_fresh_binary};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Script reporting a well-formed identity whose git head matches no
/// real commit: a foreign build from another checkout.
const FOREIGN_HEAD_SCRIPT: &str = "#!/bin/sh\n\
if [ \"$1\" = \"--build-id\" ]; then\n\
  echo 'cargo-rudzio-build-id git-head=0000000000000000000000000000000000000000 git-dirty=clean version=0.0.0 build-stamp=x profile=debug'\n\
  exit 0\n\
fi\n\
exit 1\n";

/// Script that rejects `--build-id` the way a cargo-rudzio built before
/// the flag existed does: "unknown subcommand", exit 2.
const PREDATES_FLAG_SCRIPT: &str = "#!/bin/sh\n\
echo \"cargo-rudzio: unknown subcommand $1\" >&2\n\
exit 2\n";

/// Script reporting an `unknown` git head: a build with no identity.
const UNKNOWN_HEAD_SCRIPT: &str = "#!/bin/sh\n\
if [ \"$1\" = \"--build-id\" ]; then\n\
  echo 'cargo-rudzio-build-id git-head=unknown git-dirty=unknown version=0.0.0 build-stamp=x profile=debug'\n\
  exit 0\n\
fi\n\
exit 1\n";

/// Write an executable shell script at `dir/name` and return its path.
fn write_executable(dir: &Path, name: &str, body: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join(name);
    fs::write(&path, body)?;
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions)?;
    Ok(path)
}

/// Run the guard against a fake binary that must be rejected and return
/// the rejection message. A fake that passes is itself a test failure.
fn rejection_message(dir: &Path, name: &str, body: &str) -> anyhow::Result<String> {
    let fake = write_executable(dir, name, body)?;
    match assert_fresh_binary(&fake) {
        Ok(accepted) => Err(anyhow::anyhow!(
            "freshness guard accepted fake binary {}",
            accepted.display()
        )),
        Err(err) => Ok(format!("{err:#}")),
    }
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        FOREIGN_HEAD_SCRIPT, IsolatedCargoTest, PREDATES_FLAG_SCRIPT, UNKNOWN_HEAD_SCRIPT,
        assert_fresh_binary, rejection_message,
    };

    #[rudzio::test]
    fn freshly_built_binary_passes_freshness_guard(_ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let bin = assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;
        anyhow::ensure!(
            bin.file_name().is_some_and(|name| name == "cargo-rudzio"),
            "expected the cargo-rudzio binary, got {}",
            bin.display(),
        );
        Ok(())
    }

    #[rudzio::test]
    fn foreign_head_binary_is_rejected(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let message = rejection_message(ctx.suite_tempdir(), "fake-foreign", FOREIGN_HEAD_SCRIPT)?;
        anyhow::ensure!(
            message.contains("stale/foreign"),
            "expected a stale/foreign rejection, got: {message}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn unknown_head_binary_is_rejected(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let message = rejection_message(ctx.suite_tempdir(), "fake-unknown", UNKNOWN_HEAD_SCRIPT)?;
        anyhow::ensure!(
            message.contains("reports no git-head"),
            "expected an unknown-head rejection, got: {message}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn binary_without_build_id_flag_is_rejected(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let message =
            rejection_message(ctx.suite_tempdir(), "fake-predates", PREDATES_FLAG_SCRIPT)?;
        anyhow::ensure!(
            message.contains("predates --build-id"),
            "expected a predates-flag rejection, got: {message}",
        );
        Ok(())
    }
}
