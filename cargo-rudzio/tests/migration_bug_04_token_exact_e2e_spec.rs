//! Faithful e2e reproducer for Bug #4 against the operator's **exact**
//! `token` crate shape: a lib-only `token` member in a `crates/*` workspace,
//! `rudzio` declared as a workspace-inherited **git** dependency, an inline
//! `#[rudzio::suite]` inside `src/asm/backend.rs`, a pub module named like
//! the crate (`pub mod token;`), and `[profile.dev] panic = "abort"` at the
//! workspace root.
//!
//! # Symptom (pre-fix)
//!
//! `cargo rudzio test -p token` catches a panicking test and reports it as an
//! ordinary failure (`test result: FAILED. … 1 panicked`), exiting 1 instead
//! of aborting with SIGABRT/134. The runner's catch layer (`catch_unwind`)
//! only works under `panic = "unwind"`, so catching the panic proves the
//! bridge test body was compiled with `-C panic=unwind`, not `-C panic=abort`.
//!
//! # Root cause
//!
//! cargo-rudzio re-emits the consumer's `[profile.dev] panic = "abort"` into
//! the generated aggregator manifest. But cargo's config hierarchy ranks a
//! `.cargo/config.toml` **above** the manifest `[profile]`: when the inner
//! `cargo build` walks up from the consumer's workspace root and finds an
//! ancestor config with `[profile.dev] panic = "unwind"` (exactly what the
//! rudzio repo's own `.cargo/config.toml` pins, so the framework's self-tests
//! can unwind), that config silently overrides the re-emitted `panic = "abort"`
//! and the bridge compiles with unwind.
//!
//! This spec reproduces that ancestor config by writing the rudzio repo's
//! `[profile.*] panic = "unwind"` table into a `.cargo/config.toml` in the
//! fixture's **parent** directory — the same config hierarchy position the
//! rudzio repo occupies when a consumer workspace lives anywhere under it.
//!
//! # Assertion (flips GREEN once fixed)
//!
//! With the consumer's `panic` mirrored onto the inner cargo via
//! `--config profile.<p>.panic="<v>"` (the highest-precedence config layer),
//! the panicking test must abort the whole runner: SIGABRT (signal 6), no
//! clean exit code.

#![cfg(unix)]

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, assert_fresh_binary, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread;

/// The checkout's real HEAD SHA, used as the `rev` pin so the git dependency
/// builds the rudzio code under test rather than a published version.
fn rudzio_git_rev() -> anyhow::Result<String> {
    current_git_head()
        .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))
}

/// The absolute `file://` git URL for the rudzio repo this test lives in.
///
/// A local `file://` source keeps the git dependency reachable without
/// network. The pre-fetch step below warms the per-test cargo home's git
/// cache so the actual `cargo rudzio test` run can stay offline.
fn rudzio_git_url() -> String {
    format!("file://{}", repo_root().display())
}

/// Materialise the operator's exact token workspace under `parent/workspace`
/// plus the ancestor `.cargo/config.toml` (rudzio repo's own `panic = "unwind"`)
/// under `parent`.
fn write_fixture(parent: &Path, rev: &str) -> io::Result<PathBuf> {
    // Ancestor config: replicate the rudzio repo's own `.cargo/config.toml`,
    // which intentionally pins `panic = "unwind"` so the framework's
    // self-tests unwind. Placed here so the inner build's config walk-up
    // finds it, exercising the Bug #4 override path.
    let cargo_dir = parent.join(".cargo");
    fs::create_dir_all(&cargo_dir)?;
    fs::write(
        cargo_dir.join("config.toml"),
        "[profile.dev]\npanic = \"unwind\"\n[profile.release]\npanic = \"unwind\"\n",
    )?;

    let root = parent.join("workspace");
    let token = root.join("crates").join("token");
    fs::create_dir_all(token.join("src").join("asm"))?;
    fs::create_dir_all(root.join(".cargo"))?;

    // Consumer's own `.cargo/config.toml` (build-dir only, no [profile]).
    fs::write(
        root.join(".cargo").join("config.toml"),
        "[build]\nbuild-dir = \"tmp/build-dir\"\n",
    )?;

    let git_url = rudzio_git_url();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"crates/*\"]\n\
             resolver = \"2\"\n\
             \n\
             [workspace.dependencies]\n\
             anyhow = \"=1.0.104\"\n\
             rudzio = {{ git = \"{git_url}\", rev = \"{rev}\", features = [\"runtime-futures\", \"runtime-compio\", \"common\"] }}\n\
             thiserror = \"=2.0.21\"\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.lints.rust]\n\
             warnings = \"deny\"\n\
             unsafe_code = \"deny\"\n\
             unexpected_cfgs = {{ level = \"warn\", check-cfg = ['cfg(rudzio_test)'] }}\n\
             \n\
             [profile.dev]\n\
             panic = \"abort\"\n\
             \n\
             [profile.release]\n\
             panic = \"abort\"\n"
        ),
    )?;

    fs::write(
        token.join("Cargo.toml"),
        "[package]\n\
         name = \"token\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         \n\
         [dependencies]\n\
         thiserror.workspace = true\n\
         \n\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n",
    )?;

    fs::write(token.join("src").join("lib.rs"), "pub mod asm;\npub mod span;\npub mod token;\n")?;
    fs::write(token.join("src").join("asm").join("mod.rs"), "pub mod backend;\n")?;
    fs::write(
        token.join("src").join("span.rs"),
        "//! Minimal span type so the token fixture compiles under `warnings = \"deny\"`.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub struct Span {\n    pub start: usize,\n    pub end: usize,\n}\n",
    )?;
    fs::write(
        token.join("src").join("token.rs"),
        "//! Minimal token type so the token fixture compiles under `warnings = \"deny\"`.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub struct Token;\n",
    )?;
    fs::write(
        token.join("src").join("asm").join("backend.rs"),
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub enum Backend {\n    Cranelift,\n}\n\
         \n\
         impl Backend {\n\
         \x20\x20\x20\x20pub fn from_name(name: &str) -> Option<Self> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20match name {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\"cranelift\" => Some(Self::Cranelift),\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20_ => None,\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n}\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         #[rudzio::suite([\n\
         \x20\x20\x20\x20(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test),\n\
         ])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use super::*;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20async fn from_name_recognized(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20panic!(\"probe\");\n\
         \x20\x20\x20\x20}\n}\n",
    )?;

    Ok(root)
}

/// Warm the per-test cargo home's git cache for the fixture's `file://` git
/// dependency, then run the fixture is done offline by the caller.
///
/// `--offline` blocks ALL git operations (including local `file://` clones),
/// so the git dependency must be fetched before the offline run. This mirrors
/// what the shared prepopulate does for registry crates, extended to the git
/// source the operator's shape introduces.
fn prepopulate_git_dep(cargo_home: &Path, fixture_root: &Path) -> anyhow::Result<()> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo_bin)
        .arg("fetch")
        .arg("--manifest-path")
        .arg(fixture_root.join("Cargo.toml"))
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_NET_OFFLINE", "false")
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(io::Error::other)?;
    anyhow::ensure!(
        output.status.success(),
        "pre-fetching the git dependency for {} failed: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        fixture_root.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// Drive `cargo rudzio test -p token` against `fixture_root` with the
/// freshness-checked cargo-rudzio binary and an offline (pre-warmed) cargo
/// home. The inner build writes to per-test target/build dirs.
fn run_cargo_rudzio(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    fixture_root: &Path,
) -> anyhow::Result<Output> {
    let binary = assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;
    let output = Command::new(binary)
        .arg("test")
        .arg("-p")
        .arg("token")
        .current_dir(fixture_root)
        .env("CARGO", env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(io::Error::other)?;
    Ok(output)
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::{
        IsolatedCargoTest, PathBuf, prepopulate_git_dep, rudzio_git_rev, run_cargo_rudzio,
        write_fixture,
    };

    /// **Bug #4** — the operator's exact token shape: `panic = "abort"` must
    /// abort the runner on a panicking test, even with the rudzio repo's own
    /// `panic = "unwind"` config sitting in the consumer's ancestor chain.
    #[rudzio::test]
    async fn panic_abort_survives_ancestor_config_override(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let parent = ctx.suite_tempdir().join("token-panic-abort-parent");
        let fixture_root = write_fixture(&parent, &rev)?;

        let cargo_build_dir: PathBuf = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        prepopulate_git_dep(ctx.cargo_home(), &fixture_root)?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &fixture_root,
        )?;

        let status = output.status;
        let aborted =
            status.signal() == Some(6_i32) || (!status.success() && status.code().is_none());
        anyhow::ensure!(
            aborted,
            "expected the runner to abort on panic (SIGABRT / signal exit) under the operator's \
             token shape, but it exited {status:?} (code={:?}, signal={:?}). The ancestor \
             `[profile.dev] panic = \"unwind\"` overrode the aggregator's re-emitted \
             `panic = \"abort\"`, so the bridge test body unwound to the catch layer.\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            status.code(),
            status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }
}
