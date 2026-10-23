//! Regression reproducer for Bug #8: bin-only workspace members never
//! aggregate their integration tests into the runner.
//!
//! A member with only `src/main.rs` (no `[lib]`) plus a `tests/x.rs`
//! integration test gets a stub bridge `src/lib.rs` from
//! `build_bridge_lib_rs` — the `tests/*.rs` proxy mounts live only in
//! the lib-bearing branch, so a bin-only member ends up with an empty
//! `TEST_TOKENS` slice and the runner panics at startup
//! (`from_raw_parts(NULL, 0)`).
//!
//! The assertion checks that the generated bridge `src/lib.rs` actually
//! mounts the integration test (`__rudzio_test_<id>`), which the
//! current code does not do.

use std::path::Path;

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{DevDepSpec, MemberPlan, Plan, RudzioLocation, RudzioSpec};
use rudzio::common::context::Suite;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;

/// Integration-test source the bin-only member should aggregate.
const INTEGRATION_TEST_SRC: &str = "#[rudzio::suite([(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test)])]\n\
mod tests {\n\
    #[rudzio::test]\n\
    async fn t(_c: &rudzio::common::context::Test) -> anyhow::Result<()> { Ok(()) }\n\
}\n";

/// A rudzio dev-dep pinned to `=0.1.2` with the `runtime-futures` +
/// `common` feature set (matches the Bug #8 register).
fn rudzio_dep() -> DevDepSpec {
    let mut dep = DevDepSpec::new("rudzio".to_owned());
    "=0.1.2".clone_into(&mut dep.version_req);
    dep.features = vec!["runtime-futures".to_owned(), "common".to_owned()];
    dep
}

/// Wrap `members` in a `Plan` whose rudzio dep points at the synthetic
/// workspace root (a `Path` location, so the generated aggregator stays
/// self-contained and needs no registry/network).
fn plan_with_members(members: Vec<MemberPlan>, workspace_root: &Path) -> anyhow::Result<Plan> {
    let root = workspace_root
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("tempdir path is not utf-8"))?;
    let rudzio_spec = RudzioSpec::new(
        RudzioLocation::Path(workspace_root.to_path_buf()),
        Vec::new(),
        true,
    );
    let mut plan = Plan::new(
        Utf8PathBuf::from(root),
        Utf8PathBuf::from("/tmp/target"),
        rudzio_spec,
    );
    plan.members = members;
    Ok(plan)
}

#[rudzio::suite([
    (runtime = FuturesThreadPool::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{INTEGRATION_TEST_SRC, plan_with_members, rudzio_dep};
    use cargo_rudzio::generate::{MemberBin, MemberPlan, write_runner};
    use std::fs;
    use tempfile::tempdir;

    #[rudzio::test]
    fn bin_only_member_mounts_integration_tests() -> anyhow::Result<()> {
        let tmp = tempdir()?;

        // Materialise a bin-only member: `src/main.rs` (no `[lib]`) plus
        // a `tests/x.rs` integration test that should be aggregated.
        let mybin = tmp.path().join("mybin");
        fs::create_dir_all(mybin.join("src"))?;
        fs::create_dir_all(mybin.join("tests"))?;
        fs::write(mybin.join("src").join("main.rs"), "fn main() {}\n")?;
        fs::write(mybin.join("tests").join("x.rs"), INTEGRATION_TEST_SRC)?;

        let mut member = MemberPlan::new("mybin".to_owned(), mybin.clone());
        member.src_lib_path = None; // bin-only: no `[lib]` target
        member.bin_targets = vec![MemberBin::new(
            "mybin".to_owned(),
            mybin.join("src").join("main.rs"),
        )];
        member.test_files = vec![mybin.join("tests").join("x.rs")];
        member.dev_deps = vec![rudzio_dep()];
        "2024".clone_into(&mut member.edition);

        let plan = plan_with_members(vec![member], tmp.path())?;
        let out_dir = tmp.path().join("aggregator");
        write_runner(&plan, &out_dir)?;

        let bridge_lib = out_dir
            .join("members")
            .join("mybin")
            .join("src")
            .join("lib.rs");
        let content = fs::read_to_string(&bridge_lib)?;

        anyhow::ensure!(
            content.contains("__rudzio_test_"),
            "bin-only member `mybin` must mount its integration tests into the bridge \
             lib.rs as `mod __rudzio_test_<id>;`, but the stub lib.rs has zero test \
             mounts. Bug #8: bin-only crates never aggregate integration tests.\n\
             --- bridge lib.rs ---\n{content}",
        );
        Ok(())
    }
}
