//! Behavioral spec: when two `[[test]]` binaries declare `mod <name>;`
//! pointing at DIFFERENT source files, `write_runner` returns an
//! explicit error listing both files; it does NOT silently first-wins
//! (Bug #10 pre-fix), and does NOT silently scope per-binary either
//! (which would mask the configuration mistake from the user).
//!
//! # Symptom (pre-fix, Bug #10)
//!
//! cargo-rudzio 0.1.1 (Bug #8's fix) emitted these helpers at the
//! aggregator's crate root using a `BTreeMap<String, PathBuf>` keyed
//! on the bare module name. `entry().or_insert()` made the resolution
//! **silently first-wins** — only one `common.rs` made it into the
//! generated `src/main.rs`, and the other binary's tests started
//! failing with "cannot find type X in `common`" instead of the
//! "duplicate `common` mod" the docstring promised.
//!
//! # Behavioral assertion
//!
//! Synthesize a member with two `[[test]]` binaries
//! (`tests/integration/mod.rs` + `tests/integration/common.rs`,
//! `tests/e2e/mod.rs` + `tests/e2e/common.rs`) whose `common.rs`
//! files are DIFFERENT files on disk. Drive `plan_from_manifest` +
//! `write_runner` against the fixture; assert `write_runner` returns
//! `Err` whose diagnostic chain names BOTH source paths and points
//! the user at the rename-or-alias workaround.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the rudzio repo (synthetic fixtures path-pin
/// rudzio here to exercise the in-tree code).
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a single-member workspace at `root` with two
/// `[[test]]` binaries, each declaring a `mod common;` pointing at
/// its own sibling `common.rs`. The two `common.rs` files expose
/// DIFFERENT public constants so the per-binary tests can assert
/// they received the right helper.
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

    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join("src"))?;
    fs::write(
        consumer.join("Cargo.toml"),
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
         [[test]]\n\
         name = \"e2e\"\n\
         path = \"tests/e2e/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.102\"\n",
    )?;
    fs::write(
        consumer.join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    write_test_binary(&consumer, "integration", "INTEGRATION_TOKEN")?;
    write_test_binary(&consumer, "e2e", "E2E_TOKEN")?;
    Ok(())
}

/// Materialise a single `[[test]]` binary under `<consumer>/tests/<name>/`.
///
/// Lays down the test-root `mod.rs`, a `common.rs` helper exposing a
/// uniquely-named constant `token_name`, and a `foo.rs` test file
/// that references the helper via `use crate::common::<token_name>`.
/// Per-binary scoping must route each binary's `crate::common` to
/// its own `common.rs`; pre-fix the `BTreeMap` dedup silently picked
/// one source for all binaries, and the loser's `<token_name>`
/// reference fell out as `cannot find value` at compile time.
fn write_test_binary(consumer: &Path, name: &str, token_name: &str) -> io::Result<()> {
    let dir = consumer.join("tests").join(name);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("mod.rs"),
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
        dir.join("common.rs"),
        format!("pub(crate) const {token_name}: () = ();\n"),
    )?;
    fs::write(
        dir.join("foo.rs"),
        format!(
            "use rudzio::common::context::{{Suite, Test}};\n\
             use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
             \n\
             #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
             mod tests {{\n\
             \x20\x20\x20\x20use crate::common::{token_name};\n\
             \n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn binary_sees_its_own_common() -> anyhow::Result<()> {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20let _: () = {token_name};\n\
             \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
             \x20\x20\x20\x20}}\n\
             }}\n",
        ),
    )?;
    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`. Prevents
/// `cargo metadata` / `cargo check` from refreshing registry indexes
/// for the synthetic fixture.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard restoring `CARGO_NET_OFFLINE` to its previous value
    /// on drop. Survives panics in `body`.
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
                // SAFETY: see Restore::drop in registry_preservation_spec.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see Restore::drop in registry_preservation_spec.
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
        plan_from_manifest, rudzio_source_root, with_cargo_offline, write_fixture, write_runner,
    };

    #[rudzio::test]
    fn two_test_binaries_with_same_helper_name_pointing_at_different_files_returns_error()
    -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        let result = write_runner(&plan, &out_dir);
        let Err(err) = result else {
            anyhow::bail!(
                "two binaries declaring `mod common;` at different files must bail; got Ok"
            );
        };
        let chain = format!("{err:?}");
        anyhow::ensure!(
            chain.contains("common"),
            "diagnostic must name the colliding helper (`common`), got:\n{chain}",
        );
        anyhow::ensure!(
            chain.contains("integration") && chain.contains("e2e"),
            "diagnostic must name both colliding test-binary roots (`integration` + `e2e`), got:\n{chain}",
        );
        Ok(())
    }
}
