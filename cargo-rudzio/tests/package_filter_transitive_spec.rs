//! Behavioral spec: `cargo rudzio test -p <name>` enumerates the
//! same set of crates the aggregator actually pulls in, including
//! transitive path-dep members from a sibling workspace (Bug #5).
//!
//! # Symptom (verified on cargo-rudzio 0.1.1)
//!
//! Two-workspace layout: `outer/` declares `app` as its only member;
//! `app` carries a dev-dep `inner-lib = { path = "../../inner/inner-lib" }`
//! pointing into a SEPARATE workspace `inner/` whose own member
//! `inner-lib` uses rudzio. Running `cargo rudzio test` from `outer/`
//! includes `inner-lib`'s rudzio tests in the aggregator and they
//! execute successfully. But running `cargo rudzio test -p inner-lib`
//! from `outer/` errors out:
//!
//! ```text
//! cargo-rudzio: no rudzio crates match package(s): inner-lib
//! ```
//!
//! Cause: `Plan::restrict_to_packages` matches against
//! `member.package_name`, and the plan's `members` set is constructed
//! from `metadata.workspace_members` — i.e. only DIRECT members of
//! the outer workspace. Transitive path-dep crates that get pulled
//! into the aggregator via a different code path are invisible to
//! `-p`. The two surfaces disagree on what a "member" is.
//!
//! # Behavioral assertion
//!
//! Build the two-workspace fixture, call `plan_from_manifest` against
//! `outer/Cargo.toml`, and confirm the aggregator's plan members
//! INCLUDE `inner-lib` (mirroring the no-`-p` aggregator behavior).
//! Then call `plan.restrict_to_packages(&["inner-lib".into()])` and
//! assert it succeeds with `inner-lib` retained. Pre-fix: the first
//! assertion may pass (inner-lib visible to the aggregator path) but
//! the restrict call errors. Post-fix: both succeed.
//!
//! Note: the first assertion documents the live state. If `inner-lib`
//! does NOT appear in `plan.members` at all, the bug is in plan
//! construction itself, not just in `-p` enumeration — the test
//! surfaces that as a distinct failure mode.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::plan_from_manifest;
use cargo_rudzio_test_helpers::with_cargo_offline;
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the in-tree rudzio package.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise the two-workspace layout the user reported in the
/// bug-list repro recipe. Both `app` and `inner-lib` carry rudzio
/// dev-deps so `build_plan` doesn't drop them at the gate.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    let outer = root.join("outer");
    let inner = root.join("inner");
    let app = outer.join("app");
    let inner_lib = inner.join("inner-lib");
    fs::create_dir_all(app.join("src"))?;
    fs::create_dir_all(app.join("tests").join("integration"))?;
    fs::create_dir_all(inner_lib.join("src"))?;
    fs::create_dir_all(inner_lib.join("tests"))?;
    fs::write(
        outer.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"app\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", default-features = false, features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        inner.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"inner-lib\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", default-features = false, features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        app.join("Cargo.toml"),
        "[package]\n\
         name = \"app\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"app\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[test]]\n\
         name = \"integration\"\n\
         path = \"tests/integration/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         inner-lib = { path = \"../../inner/inner-lib\" }\n",
    )?;
    fs::write(app.join("src").join("lib.rs"), "// rudzio_test marker\n")?;
    fs::write(
        app.join("tests").join("integration").join("mod.rs"),
        "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
    )?;
    fs::write(
        inner_lib.join("Cargo.toml"),
        "[package]\n\
         name = \"inner-lib\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"inner_lib\"\n\
         path = \"src/lib.rs\"\n\
         harness = false\n\
         \n\
         [[test]]\n\
         name = \"conn\"\n\
         path = \"tests/conn.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n",
    )?;
    fs::write(
        inner_lib.join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    fs::write(
        inner_lib.join("tests").join("conn.rs"),
        "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
    )?;
    Ok(())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use std::collections::BTreeSet;

    use super::{plan_from_manifest, rudzio_source_root, with_cargo_offline, write_fixture};

    #[rudzio::test]
    fn restrict_to_packages_accepts_transitive_path_dep_member() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let outer_manifest = temp.path().join("outer").join("Cargo.toml");
        let mut plan = with_cargo_offline(|| plan_from_manifest(&outer_manifest))?;
        // The aggregator pulls `inner-lib` into the runner without
        // `-p`; `Plan::restrict_to_packages` must enumerate the same
        // set. If `inner-lib` is absent from `plan.members` already,
        // the failure mode is plan-construction (not -p) but the
        // test still surfaces it as a distinct assertion.
        let package_names: Vec<String> = plan
            .members
            .iter()
            .map(|member| member.package_name.clone())
            .collect();
        anyhow::ensure!(
            package_names.iter().any(|name| name == "inner-lib"),
            "transitive path-dep `inner-lib` missing from plan.members; \
             `-p inner-lib` cannot match what was never added. \
             Members in plan: {package_names:?}",
        );
        // Now the user-visible bug: -p rejects a member the
        // aggregator already includes. Pre-fix: this call errors
        // because the filter logic is misaligned with the plan
        // construction. Post-fix: it succeeds with inner-lib
        // retained.
        plan.restrict_to_packages(&["inner-lib".to_owned()])?;
        let retained: Vec<String> = plan
            .members
            .iter()
            .map(|member| member.package_name.clone())
            .collect();
        anyhow::ensure!(
            retained == vec!["inner-lib".to_owned()],
            "after `-p inner-lib`, plan must retain exactly `inner-lib`; got {retained:?}",
        );
        Ok(())
    }

    /// Multi-package `-p`: select both `app` AND `inner-lib`
    /// together. Both must remain in the plan after the filter.
    /// Guards against an implementation that collapses to single-
    /// package semantics when more than one name is given.
    #[rudzio::test]
    fn restrict_to_packages_with_multiple_names_keeps_all_matches() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let outer_manifest = temp.path().join("outer").join("Cargo.toml");
        let mut plan = with_cargo_offline(|| plan_from_manifest(&outer_manifest))?;
        plan.restrict_to_packages(&["app".to_owned(), "inner-lib".to_owned()])?;
        let retained: BTreeSet<String> = plan
            .members
            .iter()
            .map(|member| member.package_name.clone())
            .collect();
        anyhow::ensure!(
            retained.contains("app") && retained.contains("inner-lib"),
            "multi-package `-p app -p inner-lib` must retain both; got {retained:?}",
        );
        anyhow::ensure!(
            retained.len() == 2,
            "filter must not pull in extras beyond what was named; got {retained:?}",
        );
        Ok(())
    }

    /// `-p <unknown-name>` against a fixture where the name does
    /// NOT exist in the plan must return an explicit error naming
    /// the typo, not a silent zero-test run.
    #[rudzio::test]
    fn restrict_to_packages_rejects_unknown_name_with_clear_error() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let outer_manifest = temp.path().join("outer").join("Cargo.toml");
        let mut plan = with_cargo_offline(|| plan_from_manifest(&outer_manifest))?;
        let result = plan.restrict_to_packages(&["definitely_not_a_real_member".to_owned()]);
        let Err(err) = result else {
            anyhow::bail!("`-p definitely_not_a_real_member` must error; got Ok");
        };
        let chain = format!("{err:?}");
        anyhow::ensure!(
            chain.contains("definitely_not_a_real_member"),
            "diagnostic must name the unknown package so the user can spot the typo; got:\n{chain}",
        );
        Ok(())
    }

    /// `--exclude` on a transitive path-dep member. If `-p`
    /// enumeration includes path-dep members (the Bug #5 fix), so
    /// must `--exclude`. Exclude `inner-lib` from `outer`; the
    /// remaining plan still contains `app` but not `inner-lib`.
    #[rudzio::test]
    fn exclude_packages_drops_transitive_path_dep_member() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let outer_manifest = temp.path().join("outer").join("Cargo.toml");
        let mut plan = with_cargo_offline(|| plan_from_manifest(&outer_manifest))?;
        // Pre-filter sanity: inner-lib must be in the plan (else
        // the exclude test is trivially passing).
        let pre_filter: BTreeSet<String> = plan
            .members
            .iter()
            .map(|member| member.package_name.clone())
            .collect();
        anyhow::ensure!(
            pre_filter.contains("inner-lib"),
            "fixture precondition: `inner-lib` must appear in the plan before exclude; got {pre_filter:?}",
        );
        plan.exclude_packages(&["inner-lib".to_owned()])?;
        let retained: BTreeSet<String> = plan
            .members
            .iter()
            .map(|member| member.package_name.clone())
            .collect();
        anyhow::ensure!(
            !retained.contains("inner-lib"),
            "`--exclude inner-lib` must drop the transitive path-dep; retained {retained:?}",
        );
        anyhow::ensure!(
            retained.contains("app"),
            "`--exclude inner-lib` must NOT drop the direct member `app`; retained {retained:?}",
        );
        Ok(())
    }
}
