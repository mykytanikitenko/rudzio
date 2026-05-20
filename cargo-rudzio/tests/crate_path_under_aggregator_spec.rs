//! Behavioral spec: `use crate::<helper>` inside member test files
//! resolves identically under `cargo rudzio test` and stock
//! `cargo test`.
//!
//! # Symptom (pre-fix)
//!
//! Under stock cargo a member's `[[test]] integration` binary has
//! `tests/integration/mod.rs` as its crate root, so when `mod.rs`
//! declares `mod common;` the sibling `tests/integration/some_test.rs`
//! can `use crate::common::helper` and it resolves. Under the rudzio
//! aggregator the same `mod.rs` is pulled in via `#[path]` as
//! `<member>::integration`, so `crate::common` from `some_test.rs`
//! now looks at the aggregator's crate root — which has no `common`
//! mod — and compilation fails with `unresolved import crate::common`.
//!
//! Workaround pre-fix: rewrite every `use crate::X` to
//! `use super::super::...::X` (depth-aware). Not transparent.
//!
//! # Behavioral assertion
//!
//! Synthesize a member whose `tests/integration/mod.rs` declares
//! `mod common;` and a sibling `tests/integration/foo.rs` that
//! `use crate::common::FORTY_TWO`. Drive `plan_from_manifest` +
//! `write_runner`, then compile the aggregator with `cargo build`
//! and assert the build succeeds (no `unresolved import` error). The
//! transparent fix re-exports each member's test-root helper mods at
//! the aggregator's crate root so `crate::common` resolves the same
//! way it did under stock cargo.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Workspace root of the rudzio repo (so synthetic fixtures can
/// `path = <this>` rudzio and exercise the in-tree code).
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a single-member workspace at `root` whose integration
/// test entry-point at `tests/integration/mod.rs` declares
/// `mod common;` (private) and a sibling `tests/integration/foo.rs`
/// that uses `use crate::common::FORTY_TWO`. Under stock cargo this
/// builds; pre-fix under the aggregator it does not.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;

    let consumer_tests = root.join("consumer").join("tests").join("integration");
    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::create_dir_all(&consumer_tests)?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[test]]\n\
         name = \"integration\"\n\
         path = \"tests/integration/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.102\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    // `tests/integration/mod.rs` — the test-binary crate root under
    // stock cargo, declaring `mod common;` (private helper) and
    // pulling in `mod foo;` which exercises `use crate::common`.
    fs::write(
        consumer_tests.join("mod.rs"),
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         mod common;\n\
         mod foo;\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;
    fs::write(
        consumer_tests.join("common.rs"),
        "//! Shared test helpers. Under stock cargo this lives at the\n\
         //! integration test binary's crate root via `mod common;` in\n\
         //! `mod.rs`. Under the aggregator the transparent re-export\n\
         //! must make `crate::common::FORTY_TWO` resolve identically.\n\
         pub(crate) const FORTY_TWO: u32 = 42;\n",
    )?;
    fs::write(
        consumer_tests.join("foo.rs"),
        "use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use crate::common::FORTY_TWO;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn crate_path_resolves_under_aggregator() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(FORTY_TWO == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`. Prevents
/// `cargo metadata` / `cargo build` from reaching the network for
/// the synthetic fixture.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard that restores `CARGO_NET_OFFLINE` to its previous
    /// value when dropped. Survives panics in `body`.
    struct Restore {
        /// Value observed before the guard installed; `None` = was unset.
        prev: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                #[expect(
                    unsafe_code,
                    reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see module-level comment on with_cargo_offline.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see module-level comment on with_cargo_offline.
                unsafe {
                    env::remove_var("CARGO_NET_OFFLINE");
                }
            }
        }
    }
    let prev = env::var("CARGO_NET_OFFLINE").ok();
    let _guard = Restore { prev };
    #[expect(
        unsafe_code,
        reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
    )]
    // SAFETY: see Restore::drop.
    unsafe {
        env::set_var("CARGO_NET_OFFLINE", "true");
    }
    body()
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        Command, Output, TempDir, io, plan_from_manifest, rudzio_source_root, with_cargo_offline,
        write_fixture, write_runner,
    };

    /// Drive `plan_from_manifest` + `write_runner` against the
    /// synthetic fixture, then `cargo check --manifest-path
    /// <out_dir>/Cargo.toml` to verify the aggregator's generated
    /// code is syntactically and semantically valid Rust. Returns
    /// the captured stderr (used for the assertion diagnostic on
    /// failure) and the cargo exit status.
    fn build_aggregator() -> anyhow::Result<(TempDir, Output)> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;

        // `cargo check` over `cargo build` keeps the test under one
        // minute: we only need the type-resolution pass to succeed,
        // not the link step.
        let output = with_cargo_offline(|| -> io::Result<Output> {
            Command::new("cargo")
                .arg("check")
                .arg("--manifest-path")
                .arg(out_dir.join("Cargo.toml"))
                .output()
        })?;
        Ok((temp, output))
    }

    #[rudzio::test]
    fn crate_path_resolves_to_test_root_helper_under_aggregator() -> anyhow::Result<()> {
        let (_temp, output) = build_aggregator()?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        anyhow::ensure!(
            output.status.success(),
            "cargo check of aggregator must succeed; exit = {:?}\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout}",
            output.status.code()
        );
        anyhow::ensure!(
            !stderr.contains("unresolved import") && !stderr.contains("`crate::common`"),
            "aggregator emitted an `unresolved import crate::common` (transparent re-export missing):\n--- stderr ---\n{stderr}",
        );
        Ok(())
    }
}
