//! Behavioral spec: a bare `use crate::<helper>;` (no `as` alias) in
//! a member test file must continue to bind `<helper>` as the local
//! name after the proxy rewrite (Bug #12).
//!
//! # Symptom (pre-fix)
//!
//! Aggregator codegen rewrites `use crate::common;` to
//! `use crate::__rudzio__<member>__<bin>__common;` and emits it
//! verbatim. The local binding now spells out the synthesised name —
//! body references like `common::Helper` fail with
//! `error[E0433]: cannot find module or crate 'common' in this scope`.
//! Workaround: hand-rewrite the source to `use crate::common as common;`.
//!
//! # Behavioral assertion
//!
//! Synthesise a single-member fixture whose
//! `tests/integration/mod.rs` declares `mod common;` (a sibling helper
//! defining `pub(crate) struct Helper;`) and `mod foo;`, where `foo.rs`
//! uses the bare `use crate::common;` form and then references
//! `common::Helper` from a `#[rudzio::test]` body. The proxy file
//! emitted under `<out>/src/proxies/<id>/foo.rs` must carry the
//! `as common` alias so the local binding survives the rewrite, and a
//! `cargo check` of the aggregator must succeed end-to-end.
//!
//! One further regression case is covered: explicit `as` aliases
//! (`use crate::common as renamed;`) must not regress — the fix only
//! injects `as <name>` for *bare* `UseTree::Name` shapes.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Workspace root of the in-tree rudzio package.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a single-member workspace at `root` whose integration
/// test entry-point at `tests/integration/mod.rs` declares
/// `mod common;` and `mod foo;`. `foo.rs` carries the three import
/// shapes the spec needs to differentiate. The helper `common.rs`
/// defines a `pub(crate) struct Helper` (with a constructor) and a
/// `pub(crate) const FORTY_TWO: u32 = 42;` so the body has both a
/// type-level and a value-level reference to exercise.
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
         anyhow = \"=1.0.104\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
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
        "//! Shared test helpers; pre-fix proxy rewrite drops the local\n\
         //! binding when `foo.rs` does `use crate::common;` bare.\n\
         pub(crate) struct Helper;\n\
         \n\
         impl Helper {\n\
         \x20\x20\x20\x20pub(crate) const fn new() -> Self { Self }\n\
         }\n\
         \n\
         pub(crate) const FORTY_TWO: u32 = 42;\n",
    )?;
    fs::write(
        consumer_tests.join("foo.rs"),
        // Two import shapes:
        //   1. `use crate::common;`               — bare Name, Bug #12 trigger
        //   2. `use crate::common as renamed;`    — explicit Rename, regression
        //
        // Body inside `mod tests` references each via `super::*`.
        "use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         use crate::common;\n\
         use crate::common as renamed;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use super::{common, renamed};\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn bare_use_binding_resolves() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let _helper: common::Helper = common::Helper::new();\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let _renamed: renamed::Helper = renamed::Helper::new();\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(common::FORTY_TWO == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`. Prevents
/// `cargo metadata` / `cargo check` from reaching the network.
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
        Command, Output, Path, PathBuf, TempDir, fs, io, plan_from_manifest, rudzio_source_root,
        with_cargo_offline, write_fixture, write_runner,
    };

    /// Drive `plan_from_manifest` + `write_runner` against the
    /// synthetic fixture and return the temp dir + the path to the
    /// emitted aggregator output dir. Tests inspect the proxy files
    /// directly and/or `cargo check` the aggregator.
    fn write_aggregator() -> anyhow::Result<(TempDir, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        Ok((temp, out_dir))
    }

    /// Locate the proxy `foo.rs` emitted under any bridge's
    /// `src/proxies/<id>/` directory. The `<id>` is synth-derived per
    /// binary; we scan rather than guess so a future change in the
    /// id-derivation scheme doesn't false-fail this test.
    ///
    /// Under v2 proxies live inside each bridged member's directory
    /// (`<out>/members/<bridge_dir>/src/proxies/<id>/`), not directly
    /// under the aggregator's `src/proxies/` — the scan walks every
    /// bridge directory so the test stays agnostic to which member
    /// owns the test binary.
    fn find_proxy_foo(out_dir: &Path) -> anyhow::Result<PathBuf> {
        let members_dir = out_dir.join("members");
        let mut hits: Vec<PathBuf> = Vec::new();
        for bridge_entry in fs::read_dir(&members_dir)? {
            let bridge_path = bridge_entry?.path();
            if !bridge_path.is_dir() {
                continue;
            }
            let proxies = bridge_path.join("src").join("proxies");
            let Ok(proxies_iter) = fs::read_dir(&proxies) else {
                continue;
            };
            for entry in proxies_iter {
                let path = entry?.path();
                if path.is_dir() {
                    let candidate = path.join("foo.rs");
                    if candidate.exists() {
                        hits.push(candidate);
                    }
                }
            }
        }
        anyhow::ensure!(
            hits.len() == 1,
            "expected exactly one proxy foo.rs under any bridge's src/proxies/, found {hits:?} under {}",
            members_dir.display()
        );
        hits.into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("hits.len() == 1 checked above"))
    }

    /// Run `cargo check` against the aggregator's Cargo.toml.
    fn cargo_check(out_dir: &Path) -> io::Result<Output> {
        with_cargo_offline(|| -> io::Result<Output> {
            Command::new("cargo")
                .arg("check")
                .arg("--manifest-path")
                .arg(out_dir.join("Cargo.toml"))
                .output()
        })
    }

    /// **Bug #12 core**: bare `use crate::common;` must produce a
    /// proxy import that preserves the local binding `common`. After
    /// the fix the proxy emits
    /// `use crate::__rudzio__<...>__common as common;`. Pre-fix it
    /// emits `use crate::__rudzio__<...>__common;` with no alias —
    /// the local binding is the synth name and body references to
    /// `common::*` fail to resolve.
    #[rudzio::test]
    fn bare_use_crate_helper_emits_as_alias_in_proxy() -> anyhow::Result<()> {
        let (_temp, out_dir) = write_aggregator()?;
        let proxy_foo = find_proxy_foo(out_dir.as_path())?;
        let source = fs::read_to_string(&proxy_foo)?;
        // The synth name varies (member + binary id derivation), but
        // the *as common* tail is invariant after the fix.
        anyhow::ensure!(
            source.contains("__common as common"),
            "proxy must alias the synth import back to `common`; full source:\n{source}",
        );
        Ok(())
    }

    /// Regression: explicit `use crate::common as renamed;` must
    /// continue to bind `renamed` after the rewrite. The fix for
    /// Bug #12 introduces an `as` alias only when the source had no
    /// rename; existing renames must pass through untouched.
    #[rudzio::test]
    fn explicit_rename_use_preserves_user_alias_in_proxy() -> anyhow::Result<()> {
        let (_temp, out_dir) = write_aggregator()?;
        let proxy_foo = find_proxy_foo(out_dir.as_path())?;
        let source = fs::read_to_string(&proxy_foo)?;
        anyhow::ensure!(
            source.contains("__common as renamed"),
            "explicit `as renamed` alias must survive the rewrite; full source:\n{source}",
        );
        // Belt-and-braces: the user's `renamed` binding must not be
        // shadowed by an auto-emitted `as common` for the same import.
        anyhow::ensure!(
            !source.contains("as renamed as common") && !source.contains("as common as renamed"),
            "rename must not be double-aliased; full source:\n{source}",
        );
        Ok(())
    }

    /// End-to-end behavioural assertion: the aggregator's
    /// `cargo check` must succeed. Pre-fix this fails with
    /// `error[E0433]: cannot find module or crate 'common' in this
    /// scope`. Post-fix the proxy import binds `common` to the synth
    /// path and the body's `common::Helper` reference resolves.
    #[rudzio::test]
    fn aggregator_builds_with_bare_use_crate_helper() -> anyhow::Result<()> {
        let (_temp, out_dir) = write_aggregator()?;
        let output = cargo_check(out_dir.as_path())?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        anyhow::ensure!(
            output.status.success(),
            "cargo check of aggregator must succeed; exit = {:?}\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout}",
            output.status.code()
        );
        anyhow::ensure!(
            !stderr.contains("cannot find module or crate `common`")
                && !stderr.contains("unresolved import"),
            "aggregator must not emit E0433 for `common` after Bug #12 fix:\n--- stderr ---\n{stderr}",
        );
        Ok(())
    }
}
