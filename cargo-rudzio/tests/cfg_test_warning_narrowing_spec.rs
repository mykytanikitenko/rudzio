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

/// Materialise the workspace skeleton (Cargo.tomls and the consumer
/// src dir) at `root`. Does NOT write any source files — callers
/// write whatever per-scenario source they want under
/// `<root>/consumer/src/`.
fn write_workspace_skeleton(root: &Path, rudzio_root: &Path) -> io::Result<()> {
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
    Ok(())
}

/// Materialise a single-member workspace at `root` whose `src/lib.rs`
/// has both shapes the warning narrowing must distinguish (the
/// original Bug #4 repro fixture: a helper-only mod and a
/// test-bearing mod).
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    write_workspace_skeleton(root, rudzio_root)?;
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

/// Materialise a workspace at `root` with a single `consumer/src/lib.rs`
/// holding `lib_rs_content`. Used by Bug #4 defensive tests to drive
/// the scanner against per-scenario source shapes.
fn write_fixture_with_lib_content(
    root: &Path,
    rudzio_root: &Path,
    lib_rs_content: &str,
) -> io::Result<()> {
    write_workspace_skeleton(root, rudzio_root)?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        lib_rs_content,
    )?;
    Ok(())
}

/// Build a plan for the fixture at `root` and return its warning
/// vector. Centralises the chain of `plan_from_manifest`,
/// `with_cargo_offline`, and `scan_unbroadened_cfg_test_mods_in_plan`,
/// so each test fn stays focused on its assertion.
fn warnings_for(root: &Path) -> anyhow::Result<Vec<String>> {
    let manifest = root.join("Cargo.toml");
    let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
    Ok(scan_unbroadened_cfg_test_mods_in_plan(&plan))
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
        warnings_for, with_cargo_offline, write_fixture, write_fixture_with_lib_content,
        write_workspace_skeleton,
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

    /// **Bug #4 enhancement** regression: the warning text must
    /// describe the FULL migration recipe, not just the cfg
    /// broadening step.
    ///
    /// # Symptom (pre-enhancement)
    ///
    /// The warning emitted for a test-bearing `#[cfg(test)] mod` only
    /// mentions the cfg-broadening step:
    ///
    /// ```text
    /// Broaden to `#[cfg(any(test, rudzio_test))]`, or add a
    /// `// rudzio_test` marker comment to silence this warning when
    /// the tests are intentionally cargo-only.
    /// ```
    ///
    /// A user who follows ONLY that suggestion still ends up with the
    /// tests INVISIBLE to rudzio: the compile passes, but
    /// `cargo rudzio test it_adds` reports `0 total; 1 filtered out`
    /// because the `#[test]` / `#[tokio::test]` attributes are NOT
    /// `#[rudzio::test]` and the mod isn't wrapped in
    /// `#[rudzio::suite([(...)])]`. The framework's runner sees zero
    /// registered tests in that mod.
    ///
    /// # Behavioral assertion
    ///
    /// The emitted warning for a test-bearing mod must mention every
    /// step required to make those tests visible under rudzio:
    ///
    ///  * `#[cfg(any(test, rudzio_test))]` — broaden the gate
    ///  * `#[rudzio::suite(`                 — wrap the mod
    ///  * `#[rudzio::test]`                  — replace each `#[test]`
    ///
    /// Anything less leaves the user with a passing compile but
    /// invisible tests, which is worse than the original warning's
    /// silence (the user thinks they migrated when they didn't).
    #[rudzio::test]
    fn warning_text_includes_full_migration_recipe() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let warnings = scan_unbroadened_cfg_test_mods_in_plan(&plan);
        let tests_warning = warnings
            .iter()
            .find(|warning| warning.contains("mod tests"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "expected one warning for `mod tests`, got none.\nall warnings:\n{warnings:#?}",
                )
            })?;

        // Every required migration step must be cited in the warning
        // text. The exact phrasing is intentionally checked as a
        // substring rather than a regex so the upstream wording fix
        // is free to choose the most readable form.
        let must_mention = [
            ("any(test, rudzio_test)", "broaden the cfg gate"),
            ("#[rudzio::suite(", "wrap the mod in a suite"),
            (
                "#[rudzio::test]",
                "replace each `#[test]` with the rudzio attribute",
            ),
        ];
        for (needle, what_it_means) in must_mention {
            anyhow::ensure!(
                tests_warning.contains(needle),
                "warning text is missing `{needle}` (which signals the user must {what_it_means}).\n\
                 Following only the cfg-broadening suggestion leaves tests INVISIBLE to rudzio — \
                 the full recipe must be in the warning so the user knows the migration is more \
                 than one step.\n\
                 actual warning:\n{tests_warning}",
            );
        }
        Ok(())
    }

    // ===========================================================
    // Bug #4 enhancement DEFENSIVE coverage — text shape, escape
    // hatch preservation, test-attribute variants, fully-migrated
    // and partial-migration cases, module shapes, polarity.
    // ===========================================================

    /// Defensive: the warning text must list its steps in an
    /// enumerated form (numbered or bulleted) so a user can read it
    /// as a recipe rather than a wall of prose. The fix is text-only
    /// — without an enforced structure, future edits could degrade
    /// readability back to a single-paragraph blob.
    #[rudzio::test]
    fn warning_text_uses_enumerated_form_for_migration_steps() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture(temp.path(), &rudzio_source_root())?;
        let warnings = warnings_for(temp.path())?;
        let tests_warning = warnings
            .iter()
            .find(|warning| warning.contains("mod tests"))
            .ok_or_else(|| anyhow::anyhow!("expected `mod tests` warning, got none"))?;
        // Accept any of several enumeration markers so the upstream
        // fix can choose its preferred style (1./2., bullets, dashes).
        // Reject single-paragraph prose with no structural cue.
        let enumeration_markers = ["1.", "1)", "(1)", "* ", "- ", "\u{2022}"];
        let has_enumeration = enumeration_markers
            .iter()
            .any(|marker| tests_warning.contains(marker));
        anyhow::ensure!(
            has_enumeration,
            "warning text must enumerate the migration steps (1./2./3., bullets, or similar) \
             so users can follow a recipe rather than parse prose. None of the markers \
             {enumeration_markers:?} appear.\nactual warning:\n{tests_warning}",
        );
        Ok(())
    }

    /// Defensive: the warning text must still mention the
    /// silence-by-marker escape hatch (`// rudzio_test` comment).
    /// Regression for the user who intentionally keeps tests
    /// cargo-only — losing this hint forces them to find it elsewhere.
    #[rudzio::test]
    fn warning_still_mentions_marker_silence_option() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture(temp.path(), &rudzio_source_root())?;
        let warnings = warnings_for(temp.path())?;
        let tests_warning = warnings
            .iter()
            .find(|warning| warning.contains("mod tests"))
            .ok_or_else(|| anyhow::anyhow!("expected `mod tests` warning, got none"))?;
        anyhow::ensure!(
            tests_warning.contains("rudzio_test"),
            "warning must still mention the `// rudzio_test` marker comment escape hatch \
             for intentionally cargo-only tests.\nactual warning:\n{tests_warning}",
        );
        anyhow::ensure!(
            tests_warning.contains("marker") || tests_warning.contains("silence"),
            "warning must use the words `marker` or `silence` so users can search for \
             the escape hatch.\nactual warning:\n{tests_warning}",
        );
        Ok(())
    }

    /// Defensive: the warning must continue to include the
    /// `file:line:` source pointer and the mod identifier so the
    /// diagnostic stays actionable post-rewording.
    #[rudzio::test]
    fn warning_still_includes_file_line_and_mod_name() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture(temp.path(), &rudzio_source_root())?;
        let warnings = warnings_for(temp.path())?;
        let tests_warning = warnings
            .iter()
            .find(|warning| warning.contains("mod tests"))
            .ok_or_else(|| anyhow::anyhow!("expected `mod tests` warning, got none"))?;
        anyhow::ensure!(
            tests_warning.contains("lib.rs"),
            "warning must include the source file path so the user can jump to it.\n\
             actual warning:\n{tests_warning}",
        );
        anyhow::ensure!(
            tests_warning.contains(':') && tests_warning.matches(':').count() >= 2,
            "warning must include a `file:line:` pointer.\nactual warning:\n{tests_warning}",
        );
        anyhow::ensure!(
            tests_warning.contains("mod tests"),
            "warning must name the offending mod.\nactual warning:\n{tests_warning}",
        );
        Ok(())
    }

    /// Defensive: `#[tokio::test]` (the most common async test
    /// attribute) inside a `#[cfg(test)] mod` must trigger the
    /// warning — these are the migration cases the warning is FOR.
    #[rudzio::test]
    fn tokio_test_attribute_is_detected_as_a_test() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(test)]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[tokio::test]\n\
             \x20\x20\x20\x20async fn it_runs() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "mod with `#[tokio::test]` must emit a warning. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: `#[bench]` attribute must also trigger the warning.
    /// Benchmarks compile under `--cfg test` only and would vanish
    /// under rudzio's `--cfg rudzio_test` just like regular tests.
    #[rudzio::test]
    fn bench_attribute_is_detected_as_a_test() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(test)]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[bench]\n\
             \x20\x20\x20\x20fn bench_it(_b: &mut ()) {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "mod with `#[bench]` must emit a warning. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: `#[ignore] #[test]` combination must still trigger
    /// the warning. The `ignore` attribute is a sibling of `test`,
    /// not a replacement.
    #[rudzio::test]
    fn ignore_with_test_combination_is_detected() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(test)]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[ignore]\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn ignored_test() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "mod with `#[ignore] #[test]` must emit a warning. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: `#[should_panic] #[test]` combination must also
    /// trigger the warning.
    #[rudzio::test]
    fn should_panic_with_test_combination_is_detected() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(test)]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[should_panic]\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn panics_on_purpose() { panic!(\"by design\"); }\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "mod with `#[should_panic] #[test]` must emit a warning. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: a FULLY migrated mod — broadened cfg + suite wrap
    /// + `#[rudzio::test]` attributes — must emit NO warning. Any
    /// false positive here adds noise for users who already followed
    /// the full recipe.
    ///
    /// Note: the file-level substring skip (which fires when the
    /// source contains `rudzio::suite` or `rudzio_test` anywhere)
    /// also suppresses this warning regardless of mod shape. This
    /// test pins both layers together: the file is correctly
    /// detected as fully migrated.
    #[rudzio::test]
    fn fully_migrated_mod_emits_no_warning() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(any(test, rudzio_test))]\n\
             #[rudzio::suite([(\n\
             \x20\x20\x20\x20runtime = rudzio::runtime::tokio::Multithread::new,\n\
             \x20\x20\x20\x20suite = rudzio::common::context::Suite,\n\
             \x20\x20\x20\x20test = rudzio::common::context::Test,\n\
             )])]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn it_runs() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.is_empty(),
            "fully migrated mod must emit NO warnings. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: cfg broadened to `any(test, rudzio_test)` but no
    /// suite wrap and `#[test]` still present. This is the partial-
    /// migration case the warning's full recipe targets. Currently
    /// the scanner uses a file-level substring skip on `rudzio_test`,
    /// which suppresses the warning even when only the cfg has been
    /// broadened — so this test is EXPECTED TO FAIL pre-enhancement
    /// (no warning) and to PASS post-enhancement (the scanner stops
    /// silencing on the cfg-broadened-only shape).
    #[rudzio::test]
    fn partial_migration_cfg_broadened_without_suite_wrap_still_warns() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(any(test, rudzio_test))]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn it_runs() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "partial migration (cfg broadened but `#[test]` and no `#[rudzio::suite]`) \
             must STILL warn — the tests would still be invisible to rudzio.\n\
             got warnings:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: suite wrap present but cfg is narrow `#[cfg(test)]`
    /// (still cargo-only). The tests would never reach rudzio's
    /// runner because the mod is excluded from `--cfg rudzio_test`
    /// compilation. Must warn.
    #[rudzio::test]
    fn partial_migration_suite_wrap_without_broadened_cfg_still_warns() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(test)]\n\
             #[rudzio::suite([(\n\
             \x20\x20\x20\x20runtime = rudzio::runtime::tokio::Multithread::new,\n\
             \x20\x20\x20\x20suite = rudzio::common::context::Suite,\n\
             \x20\x20\x20\x20test = rudzio::common::context::Test,\n\
             )])]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20fn it_runs() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "partial migration (suite wrap present but cfg still narrow) must STILL warn \
             — under `cargo rudzio test` the mod is excluded entirely.\n\
             got warnings:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: file containing the literal `rudzio::suite` (even
    /// in a comment) is skipped entirely. This is an existing
    /// file-level escape hatch — preserve it.
    #[rudzio::test]
    fn file_with_rudzio_suite_literal_is_skipped() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "// rudzio::suite present in this comment -- file is skipped\n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn it_runs() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.is_empty(),
            "file containing literal `rudzio::suite` must be skipped entirely. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: `// rudzio_test` marker comment alone (without
    /// `rudzio::suite`) also silences the warning. Existing escape
    /// hatch for intentionally cargo-only test modules.
    #[rudzio::test]
    fn file_with_rudzio_test_marker_comment_is_skipped() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "// rudzio_test marker -- intentionally cargo-only\n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn it_runs() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.is_empty(),
            "file with `// rudzio_test` marker must be skipped. got:\n{warnings:#?}",
        );
        Ok(())
    }

    // ----- Module shape variations (P2) -----

    /// Defensive: `#[cfg(test)] mod inner` nested inside an outer
    /// `mod outer { ... }` must be detected. The scanner descends
    /// into nested mods.
    #[rudzio::test]
    fn nested_cfg_test_mod_inside_outer_mod_is_detected() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "pub mod outer {\n\
             \x20\x20\x20\x20#[cfg(test)]\n\
             \x20\x20\x20\x20mod inner {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20\x20\x20\x20\x20fn nested_test() {}\n\
             \x20\x20\x20\x20}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod inner")),
            "nested `#[cfg(test)] mod inner` inside `mod outer` must be detected. \
             got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: out-of-line `#[cfg(test)] mod tests;` with the
    /// body in a separate `tests.rs` file containing `#[test]` fns.
    /// The scanner must follow the out-of-line declaration to
    /// inspect the body.
    ///
    /// NOTE: pre-fix the scanner may not follow out-of-line mods at
    /// all; this test then fails because no warning is emitted. Post-
    /// fix the scanner should detect the body.
    #[rudzio::test]
    fn out_of_line_cfg_test_mod_with_separate_file_is_detected() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_workspace_skeleton(temp.path(), &rudzio_source_root())?;
        fs::write(
            temp.path().join("consumer").join("src").join("lib.rs"),
            "#[cfg(test)]\n\
             mod tests;\n",
        )?;
        fs::write(
            temp.path().join("consumer").join("src").join("tests.rs"),
            "#[test]\n\
             fn out_of_line_test() {}\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "out-of-line `#[cfg(test)] mod tests;` with test fns in `tests.rs` must be \
             detected. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: multiple independent `#[cfg(test)] mod ...` blocks
    /// in the same file each emit their own warning. The scanner
    /// must not stop at the first match.
    #[rudzio::test]
    fn multiple_cfg_test_mods_in_same_file_each_warn() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(test)]\n\
             mod unit_tests {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn unit_one() {}\n\
             }\n\
             \n\
             #[cfg(test)]\n\
             mod property_tests {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn prop_one() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings
                .iter()
                .any(|warning| warning.contains("mod unit_tests")),
            "first `#[cfg(test)] mod unit_tests` must warn. got:\n{warnings:#?}",
        );
        anyhow::ensure!(
            warnings
                .iter()
                .any(|warning| warning.contains("mod property_tests")),
            "second `#[cfg(test)] mod property_tests` must ALSO warn — scanner must not \
             stop at first match. got:\n{warnings:#?}",
        );
        Ok(())
    }

    // ----- Polarity / cfg-shape variations (P2) -----

    /// Defensive: `#[cfg(not(test))]` is the inverse predicate — the
    /// mod is gated to NON-test builds. It would be INCLUDED under
    /// `--cfg rudzio_test` (which is not `test`), so rudzio sees it
    /// fine. No warning.
    #[rudzio::test]
    fn cfg_not_test_does_not_warn() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(not(test))]\n\
             mod prod_only {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn this_is_a_typo() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            !warnings
                .iter()
                .any(|warning| warning.contains("mod prod_only")),
            "`#[cfg(not(test))]` is not a cargo-only-test gate — the warning must NOT \
             fire on it. got:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: `#[cfg(all(test, other_flag))]` — gated MORE
    /// strictly than bare `test`. The tests would still be invisible
    /// under rudzio's `--cfg rudzio_test`, so the warning must still
    /// fire.
    #[rudzio::test]
    fn cfg_all_test_other_still_warns() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(all(test, feature = \"some-flag\"))]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn gated_test() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "`#[cfg(all(test, ...))]` is gated tighter than bare `test` but STILL \
             excluded under `--cfg rudzio_test`. Must warn.\ngot:\n{warnings:#?}",
        );
        Ok(())
    }

    /// Defensive: `#[cfg(any(test, other_flag))]` with `other_flag`
    /// NOT being `rudzio_test`. This is a different broadening than
    /// the one rudzio wants — under `--cfg rudzio_test` alone (no
    /// `--cfg test`, no `--cfg other_flag`), the mod is still hidden.
    #[rudzio::test]
    fn cfg_any_test_other_without_rudzio_test_still_warns() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write_fixture_with_lib_content(
            temp.path(),
            &rudzio_source_root(),
            "#[cfg(any(test, feature = \"some-flag\"))]\n\
             mod tests {\n\
             \x20\x20\x20\x20#[test]\n\
             \x20\x20\x20\x20fn gated_test() {}\n\
             }\n",
        )?;
        let warnings = warnings_for(temp.path())?;
        anyhow::ensure!(
            warnings.iter().any(|warning| warning.contains("mod tests")),
            "`#[cfg(any(test, OTHER))]` where OTHER is not `rudzio_test` does NOT \
             expose the mod to rudzio. Must warn.\ngot:\n{warnings:#?}",
        );
        Ok(())
    }
}
