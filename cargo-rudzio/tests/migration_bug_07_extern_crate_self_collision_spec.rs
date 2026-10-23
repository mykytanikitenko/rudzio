//! Regression reproducer for Bug #7: `extern crate self as X` collides
//! with a self-named `mod X;` in a crate whose package name is `X`.
//!
//! When a lib-bearing member named `project` declares `mod project;` in
//! its own `src/lib.rs`, the generated bridge appends
//! `#[cfg(rudzio_test)] extern crate self as project;` — the crate's own
//! `mod project;` is the previous definition of the name, so rustc
//! reports E0260 (`project` defined twice).
//!
//! The assertion checks that the generated bridge `src/lib.rs` does NOT
//! emit the self-alias when it would collide with a self-named module.
//! The current code emits it unconditionally (whenever the member has
//! test proxies to mount).

use std::path::Path;

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{DevDepSpec, MemberPlan, Plan, RudzioLocation, RudzioSpec};
use rudzio::common::context::Suite;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;

/// Integration-test source the member should aggregate (provides the
/// test proxies that trigger the bridge's `extern crate self as ...`
/// emission).
const INTEGRATION_TEST_SRC: &str = "#[rudzio::suite([(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test)])]\n\
mod tests {\n\
    #[rudzio::test]\n\
    async fn t(_c: &rudzio::common::context::Test) -> anyhow::Result<()> { Ok(()) }\n\
}\n";

/// A rudzio dev-dep pinned to `=0.1.2` with the `runtime-futures` +
/// `common` feature set (matches the Bug #7 register).
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
    use cargo_rudzio::generate::{MemberPlan, write_runner};
    use std::fs;
    use tempfile::tempdir;

    #[rudzio::test]
    fn extern_crate_self_alias_avoids_self_named_module_collision() -> anyhow::Result<()> {
        let tmp = tempdir()?;

        // Materialise a lib-bearing member named `project` whose lib.rs
        // declares a self-named module `mod project;`.
        let project = tmp.path().join("project");
        fs::create_dir_all(project.join("src"))?;
        fs::create_dir_all(project.join("tests"))?;
        fs::write(project.join("src").join("lib.rs"), "mod project;\n")?;
        fs::write(
            project.join("src").join("project.rs"),
            "pub fn placeholder() {}\n",
        )?;
        fs::write(project.join("tests").join("x.rs"), INTEGRATION_TEST_SRC)?;

        let mut member = MemberPlan::new("project".to_owned(), project.clone());
        member.src_lib_path = Some(project.join("src").join("lib.rs"));
        member.has_src_rudzio_suite = true;
        member.test_files = vec![project.join("tests").join("x.rs")];
        member.dev_deps = vec![rudzio_dep()];
        "2024".clone_into(&mut member.edition);

        let plan = plan_with_members(vec![member], tmp.path())?;
        let out_dir = tmp.path().join("aggregator");
        write_runner(&plan, &out_dir)?;

        let bridge_lib = out_dir
            .join("members")
            .join("project")
            .join("src")
            .join("lib.rs");
        let content = fs::read_to_string(&bridge_lib)?;

        anyhow::ensure!(
            !content.contains("extern crate self as project;"),
            "bridge lib.rs must NOT emit `extern crate self as project;` for a crate named \
             `project` whose own lib.rs declares `mod project;` — the self-alias collides \
             with the module (E0260: name `project` defined twice). Bug #7.\n\
             --- bridge lib.rs ---\n{content}",
        );
        Ok(())
    }
}
