//! Reproduction spec for Bug #6: workspace-wide `cargo rudzio test`
//! (no `-p`) duplicates path-dep crates (~107 "multiple different
//! versions of crate `b`" errors).
//!
//! Root cause: `build_cargo_toml`'s dev-dep union loop skips
//! intra-workspace sibling deps by matching the *entry* name
//! (`dep.rename.unwrap_or(dep.name)`) against the set of member package
//! names. A RENAMED intra-workspace path dep (`b_alias = { package =
//! "b", path = "../b" }`) slips past the skip (its entry name `b_alias`
//! is not a member), reaches `render_dev_dep` →
//! `render_sibling_bridge_dep`, and emits `path = "../b"` in the
//! AGGREGATOR's `[dependencies]`. `../` is only valid from a bridge
//! manifest (`members/<x>/Cargo.toml`); from the aggregator root it
//! escapes the tree, so cargo sees the same `b_rudzio_bridge` package at
//! two paths → the duplicate.
//!
//! A plain (non-renamed) `b = { path = "../b" }` is correctly skipped
//! by the union loop, so the rename is the precise trigger boundary.
//! Both forms are asserted below: the renamed form FAILS under the
//! current pipeline; the plain form PASSES and documents the boundary.

use std::fs;
use std::path::Path;

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{
    DevDepSpec, MemberPlan, Plan, RudzioLocation, RudzioSpec, write_runner,
};
use rudzio::common::context::Suite;
use rudzio::runtime::async_std::Runtime as AsyncStdRuntime;
use rudzio::runtime::compio::Runtime as CompioRuntime;
use rudzio::runtime::embassy::Runtime as EmbassyRuntime;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;
use rudzio::runtime::smol::Runtime as SmolRuntime;
use rudzio::runtime::tokio::{
    CurrentThread as TokioCurrentThread, Local as TokioLocal, Multithread as TokioMultithread,
};

/// Construct a default-shaped rudzio dev-dep entry (name only).
fn rudzio_dep() -> DevDepSpec {
    DevDepSpec::new("rudzio".to_owned())
}

/// Materialise `<tmp>/<name>/src/lib.rs` on disk and return a
/// `MemberPlan` configured against those real paths, flagged
/// bridge-eligible (src rudzio suite present).
///
/// `write_runner` reads each bridged member's `lib.rs` to copy its
/// content into the generated bridge `src/lib.rs`, so tests driving
/// `write_runner` need real files on disk.
fn materialise_member(
    tmp: &Path,
    name: &str,
    dev_deps: Vec<DevDepSpec>,
) -> anyhow::Result<MemberPlan> {
    let member_dir = tmp.join(name);
    fs::create_dir_all(member_dir.join("src"))?;
    fs::write(member_dir.join("src").join("lib.rs"), "")?;
    let mut plan = MemberPlan::new(name.to_owned(), member_dir.clone());
    plan.dev_deps = dev_deps;
    "2024".clone_into(&mut plan.edition);
    plan.src_lib_path = Some(member_dir.join("src").join("lib.rs"));
    plan.has_src_rudzio_suite = true;
    Ok(plan)
}

/// Wrap `members` in a `Plan` rooted at `workspace_root` with a default
/// version-only rudzio spec.
fn plan_with_members(members: Vec<MemberPlan>, workspace_root: &str) -> Plan {
    let rudzio_spec = RudzioSpec::new(
        RudzioLocation::Version {
            req: "0.1".to_owned(),
            registry: None,
        },
        Vec::new(),
        true,
    );
    let mut plan = Plan::new(
        Utf8PathBuf::from(workspace_root),
        Utf8PathBuf::from("/tmp/target"),
        rudzio_spec,
    );
    plan.members = members;
    plan
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
    (runtime = TokioCurrentThread::new, suite = Suite, test = Test),
    (runtime = TokioLocal::new, suite = Suite, test = Test),
    (runtime = CompioRuntime::new, suite = Suite, test = Test),
    (runtime = EmbassyRuntime::new, suite = Suite, test = Test),
    (runtime = FuturesThreadPool::new, suite = Suite, test = Test),
    (runtime = AsyncStdRuntime::new, suite = Suite, test = Test),
    (runtime = SmolRuntime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{DevDepSpec, materialise_member, plan_with_members, rudzio_dep, write_runner};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// RENAMED intra-workspace path dep: `b_alias = { package = "b", path = "../b" }`.
    fn renamed_sibling_dep(b_dir: PathBuf) -> DevDepSpec {
        let mut dep = DevDepSpec::new("b".to_owned());
        dep.rename = Some("b_alias".to_owned());
        dep.path = Some(b_dir);
        dep
    }

    /// PLAIN intra-workspace path dep: `b = { path = "../b" }`.
    fn plain_sibling_dep(b_dir: PathBuf) -> DevDepSpec {
        let mut dep = DevDepSpec::new("b".to_owned());
        dep.path = Some(b_dir);
        dep
    }

    #[rudzio::test]
    fn renamed_sibling_path_dep_must_not_emit_parent_relative_bridge_path_in_aggregator()
    -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let b_dir = tmp.path().join("b");
        let member_a = materialise_member(
            tmp.path(),
            "a",
            vec![rudzio_dep(), renamed_sibling_dep(b_dir)],
        )?;
        let member_b = materialise_member(tmp.path(), "b", vec![rudzio_dep()])?;
        let plan = plan_with_members(
            vec![member_a, member_b],
            tmp.path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("tempdir path is not utf-8"))?,
        );
        let out = tmp.path().join("aggregator");
        write_runner(&plan, &out)?;

        let manifest = fs::read_to_string(out.join("Cargo.toml"))?;
        anyhow::ensure!(
            !manifest.contains("path = \"../"),
            "aggregator [dependencies] must not use `../`-prefixed sibling-bridge paths \
             (only `./members/...`):\n{manifest}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn plain_sibling_path_dep_is_skipped_in_aggregator_union() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let b_dir = tmp.path().join("b");
        let member_a = materialise_member(
            tmp.path(),
            "a",
            vec![rudzio_dep(), plain_sibling_dep(b_dir)],
        )?;
        let member_b = materialise_member(tmp.path(), "b", vec![rudzio_dep()])?;
        let plan = plan_with_members(
            vec![member_a, member_b],
            tmp.path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("tempdir path is not utf-8"))?,
        );
        let out = tmp.path().join("aggregator");
        write_runner(&plan, &out)?;

        let manifest = fs::read_to_string(out.join("Cargo.toml"))?;
        anyhow::ensure!(
            !manifest.contains("path = \"../"),
            "plain sibling dep must be skipped by the union loop (no `../` path):\n{manifest}"
        );
        Ok(())
    }
}
