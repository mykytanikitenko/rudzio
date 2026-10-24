//! Behavioral spec: a consumer crate declaring `rudzio` as a DIRECT
//! git dependency (`git = "…"` + `rev = "…"`) must render that same
//! git/rev spec into the generated bridge manifest — not an empty
//! `version = ""`.
//!
//! # Symptom (pre-fix)
//!
//! A consumer workspace whose member declares
//! `rudzio = { git = "https://…", rev = "<sha>", features = […] }` in
//! `[dependencies]` or `[dev-dependencies]` causes `cargo rudzio test`
//! to regenerate the bridge crate's `Cargo.toml` with
//! `rudzio = { version = "", features = […] }`. The git URL and `rev`
//! pin are silently dropped, leaving an empty version requirement that
//! cargo then rejects:
//!
//! ```text
//! error: failed to parse manifest at …/Cargo.toml
//!   could not parse …/Cargo.toml
//!   unexpected end of input while parsing a major version number
//! ```
//!
//! Only `rudzio = { workspace = true }` (workspace inheritance) worked,
//! because the workspace-inherited render path re-expands the concrete
//! location keys; the DIRECT git path fell through to the version
//! emitter with an empty `version_req`.
//!
//! # Root cause
//!
//! `DevDepSpec` already parses `git` / `rev` / `branch` / `tag`
//! (`apply_dev_dep_field`), and the aggregator's own rudzio entry
//! (`build_rudzio_inline_table` via `collect_rudzio_spec`) renders a
//! `RudzioLocation::Git` correctly. But the per-member bridge emitter
//! `render_dev_dep` had no `git` branch: a direct git dep fell through
//! to `fill_version_dep_body`, which emitted
//! `version = dep.version_req` (empty) plus `features`.
//!
//! # Behavioral assertion
//!
//! Drive the full generator pipeline (`plan_from_manifest` +
//! `write_runner` — the same entry points `cargo rudzio test` uses) on
//! a single-member workspace whose member declares rudzio as a direct
//! git dep with a real reachable rev, then read back the generated
//! bridge `Cargo.toml` and assert the `rudzio` entry carries `git` +
//! `rev` (and no empty `version = ""`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use cargo_rudzio_test_helpers::{current_git_head, repo_root};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::{
    CurrentThread as TokioCurrentThread, Multithread as TokioMultithread,
};
use tempfile::TempDir;

/// Absolute `file://` git URL for the rudzio repo this test lives in.
///
/// Uses the checkout's own git repo (the workspace root) as a reachable,
/// offline-safe git source so the direct-git declaration is faithful to
/// the operator's shape without requiring network access. The `rev` pin
/// is the checkout's real HEAD so any future full-subprocess run (not
/// just `cargo metadata --no-deps`) resolves to a real commit.
fn rudzio_git_url() -> String {
    format!("file://{}", repo_root().display())
}

/// The checkout's real HEAD SHA, used as the `rev` pin.
fn rudzio_git_rev() -> anyhow::Result<String> {
    current_git_head()
        .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))
}

/// Materialise a synthetic single-member workspace at `root` whose
/// member declares `rudzio` as a direct git dependency in
/// `[dev-dependencies]` with a real `rev` pin and a runtime feature.
///
/// The feature is required to reproduce the exact operator shape: with
/// features present the pre-fix renderer takes the
/// `fill_version_dep_body` branch and emits
/// `rudzio = { version = "", features = […] }` (without features it
/// would collapse to a bare empty version string instead).
fn write_fixture(root: &Path, git_url: &str, rev: &str) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\n\
         members = [\"consumer\"]\n\
         resolver = \"3\"\n\
         \n\
         [workspace.package]\n\
         edition = \"2024\"\n",
    )?;

    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"consumer\"\n\
             version = \"0.0.0\"\n\
             edition = {{ workspace = true }}\n\
             publish = false\n\
             \n\
             [lib]\n\
             name = \"consumer\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [dev-dependencies]\n\
             rudzio = {{ git = \"{git_url}\", rev = \"{rev}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n\
             anyhow = \"=1.0.104\"\n"
        ),
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn trivial_test(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
    (runtime = TokioCurrentThread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        PathBuf, TempDir, fs, plan_from_manifest, rudzio_git_rev, rudzio_git_url, write_fixture,
        write_runner,
    };

    /// Drive `plan_from_manifest` + `write_runner` on the git-dep
    /// fixture and return `(workspace_root, out_dir)`.
    fn drive_pipeline() -> anyhow::Result<(TempDir, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let git_url = rudzio_git_url();
        let rev = rudzio_git_rev()?;
        write_fixture(temp.path(), &git_url, &rev)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = plan_from_manifest(&manifest)?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        Ok((temp, out_dir))
    }

    #[rudzio::test]
    fn bridge_preserves_git_rev_on_direct_git_rudzio_dependency() -> anyhow::Result<()> {
        let git_url = rudzio_git_url();
        let rev = rudzio_git_rev()?;
        let (_temp, out_dir) = drive_pipeline()?;
        let bridge = out_dir.join("members").join("consumer").join("Cargo.toml");
        let text = fs::read_to_string(&bridge)
            .map_err(|err| anyhow::anyhow!("reading {}: {err}", bridge.display()))?;

        anyhow::ensure!(
            text.contains(&format!("git = \"{git_url}\"")),
            "bridge must re-emit the git URL for a direct git rudzio dep, but the generated \
             bridge Cargo.toml does not contain `git = \"{git_url}\"`:\n{text}",
        );
        anyhow::ensure!(
            text.contains(&format!("rev = \"{rev}\"")),
            "bridge must re-emit the rev pin for a direct git rudzio dep, but the generated \
             bridge Cargo.toml does not contain `rev = \"{rev}\"`:\n{text}",
        );
        anyhow::ensure!(
            !text.contains("version = \"\""),
            "bridge must NOT emit an empty version requirement for a direct git rudzio dep, \
             but the generated bridge Cargo.toml contains `version = \"\"`:\n{text}",
        );
        Ok(())
    }
}
