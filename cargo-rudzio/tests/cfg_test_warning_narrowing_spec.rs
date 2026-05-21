//! Behavioral spec: `scan_unbroadened_cfg_test_mods` only warns
//! when an unbroadened `#[cfg(test)] mod ...` actually contains test
//! functions (Bug #4).
//!
//! # Symptom (pre-fix)
//!
//! Every `src/**` file that carried a `#[cfg(test)] mod helpers { ... }`
//! block (the standard cargo idiom for unit-test-adjacent helper
//! modules — constants, fixtures, type aliases — that don't
//! themselves contain `#[test]` functions) emitted a "module may be
//! invisible under `cargo rudzio test`" warning. file-v3 reported
//! ~80 such warnings per build, `postgres` + `fathom_containers` added
//! more. Pure noise: the helper module has no tests to lose, so the
//! warning is wrong AND the volume drowned out genuine signals.
//!
//! # Behavioral assertion
//!
//! Construct a synthetic member whose `src/lib.rs` carries TWO
//! `#[cfg(test)] mod` blocks:
//!
//!  * `mod helpers` — no test fns, just `const X: u32`. Pre-fix:
//!    warning emitted. Post-fix: no warning.
//!  * `mod tests` — has `#[test] fn assert_one() {}` inside. Both
//!    pre-fix and post-fix: warning emitted (this IS a real cargo-
//!    only test that would vanish under rudzio).
//!
//! Drive `plan_from_manifest` then `scan_unbroadened_cfg_test_mods`
//! against the member, assert the helper-only mod produces zero
//! diagnostics, and the test-bearing mod produces exactly one.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, scan_unbroadened_cfg_test_mods_in_plan};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the rudzio repo. Synthetic fixtures `path =`
/// the in-tree rudzio so `cargo metadata` resolves without network.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a single-member workspace at `root` whose `src/lib.rs`
/// has both shapes the warning narrowing must distinguish.
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
    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n",
    )?;
    // Avoid the string `rudzio_test` and `rudzio::suite` anywhere
    // in this file content (including comments) -- those substrings
    // trigger the file-level skip in `scan_unbroadened_cfg_test_mods`,
    // which would mask both warnings and make the second assertion
    // pass-by-coincidence rather than measuring real behavior.
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "#[cfg(test)]\n\
         mod helpers {\n\
         \x20\x20\x20\x20pub(crate) const FORTY_TWO: u32 = 42;\n\
         }\n\
         \n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20\x20\x20\x20#[test]\n\
         \x20\x20\x20\x20fn assert_one() {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20assert_eq!(1, 1);\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard restoring `CARGO_NET_OFFLINE` on drop.
    struct Restore {
        /// Value observed before installation; `None` = was unset.
        prev: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                #[expect(
                    unsafe_code,
                    reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see other tests in this dir.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see other tests in this dir.
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
        fs, plan_from_manifest, rudzio_source_root, scan_unbroadened_cfg_test_mods_in_plan,
        with_cargo_offline, write_fixture,
    };

    #[rudzio::test]
    fn helper_only_cfg_test_mod_does_not_emit_warning() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let warnings = scan_unbroadened_cfg_test_mods_in_plan(&plan);
        // Pre-fix: 2 warnings (one per `#[cfg(test)] mod`).
        // Post-fix: 1 warning (only the test-bearing `mod tests`).
        let helper_warnings: Vec<&String> = warnings
            .iter()
            .filter(|warning| warning.contains("mod helpers"))
            .collect();
        anyhow::ensure!(
            helper_warnings.is_empty(),
            "helper-only `mod helpers` (no test fns) must NOT emit a warning, got:\n{helper_warnings:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn test_bearing_cfg_test_mod_still_emits_warning() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let warnings = scan_unbroadened_cfg_test_mods_in_plan(&plan);
        // `mod tests` carries `#[test] fn`. The warning still fires
        // because under `cargo rudzio test` the function would
        // silently vanish.
        let tests_warnings: Vec<&String> = warnings
            .iter()
            .filter(|warning| warning.contains("mod tests"))
            .collect();
        anyhow::ensure!(
            tests_warnings.len() == 1,
            "test-bearing `mod tests` must emit exactly one warning, got {} warnings:\n{warnings:#?}",
            tests_warnings.len(),
        );
        // Display the helper-files-on-disk path for the assertion
        // diagnostic without re-reading. `_lib_path` is intentionally
        // computed but unused — keeps the borrow live so `temp`
        // doesn't get dropped mid-assertion.
        let _lib_path = fs::read_to_string(temp.path().join("consumer").join("src").join("lib.rs"));
        Ok(())
    }
}
