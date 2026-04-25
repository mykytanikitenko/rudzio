use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{
    DevDepSpec, GitRef, MemberPlan, Plan, RudzioLocation, RudzioSpec, WorkspaceDepSpec,
    build_main_rs, build_rudzio_inline_table, collect_rudzio_spec,
};

fn member(name: &str, dev_deps: Vec<DevDepSpec>) -> MemberPlan {
    MemberPlan {
        package_name: name.to_owned(),
        manifest_dir: PathBuf::from("/nonexistent"),
        test_files: Vec::new(),
        bin_names: Vec::new(),
        has_lib: true,
        dev_deps,
    }
}

fn plan_with_members(members: Vec<MemberPlan>, workspace_root: &str) -> Plan {
    Plan {
        workspace_root: Utf8PathBuf::from(workspace_root),
        target_directory: Utf8PathBuf::from("/tmp/target"),
        members,
        rudzio_spec: RudzioSpec {
            location: RudzioLocation::Version("0.1".to_owned()),
            features: Vec::new(),
            uses_default_features: true,
        },
        workspace_deps: BTreeMap::new(),
    }
}

fn rudzio_dep() -> DevDepSpec {
    DevDepSpec {
        name: "rudzio".to_owned(),
        rename: None,
        version_req: String::new(),
        path: None,
        git: None,
        git_ref: None,
        features: Vec::new(),
        uses_default_features: true,
        workspace_inherited: false,
    }
}

fn ws_root() -> PathBuf {
    PathBuf::from("/tmp")
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::{
        BTreeMap, GitRef, Path, RudzioLocation, RudzioSpec, WorkspaceDepSpec,
        build_main_rs, build_rudzio_inline_table, collect_rudzio_spec, member, plan_with_members,
        rudzio_dep, ws_root,
    };
    use std::path::PathBuf;

    #[rudzio::test]
    fn direct_git_declaration_produces_git_location() -> anyhow::Result<()> {
        let mut dep = rudzio_dep();
        dep.git = Some("https://github.com/mykytanikitenko/rudzio".to_owned());
        dep.git_ref = Some(GitRef::Rev("deadbeef".to_owned()));
        dep.features = vec!["common".to_owned(), "runtime-tokio-multi-thread".to_owned()];

        let members = vec![member("file-v3", vec![dep])];
        let spec = collect_rudzio_spec(&members, &BTreeMap::new(), &ws_root())?;

        match &spec.location {
            RudzioLocation::Git { url, reference } => {
                anyhow::ensure!(url == "https://github.com/mykytanikitenko/rudzio");
                anyhow::ensure!(matches!(reference, Some(GitRef::Rev(r)) if r == "deadbeef"));
            }
            other => anyhow::bail!("expected Git location, got {other:?}"),
        }
        anyhow::ensure!(spec.features.iter().any(|f| f == "common"));
        anyhow::ensure!(spec.features.iter().any(|f| f == "runtime-tokio-multi-thread"));
        Ok(())
    }

    #[rudzio::test]
    fn workspace_inherited_resolves_git_from_workspace_deps() -> anyhow::Result<()> {
        let mut dep = rudzio_dep();
        dep.workspace_inherited = true;
        dep.features = vec!["runtime-tokio-multi-thread".to_owned()];

        let mut ws_deps = BTreeMap::new();
        let _prev = ws_deps.insert(
            "rudzio".to_owned(),
            WorkspaceDepSpec {
                version_req: None,
                path: None,
                git: Some("https://github.com/mykytanikitenko/rudzio".to_owned()),
                git_ref: Some(GitRef::Tag("v0.1.0".to_owned())),
                features: vec!["common".to_owned()],
                uses_default_features: true,
            },
        );

        let members = vec![member("storage", vec![dep])];
        let spec = collect_rudzio_spec(&members, &ws_deps, &ws_root())?;

        match &spec.location {
            RudzioLocation::Git { url, reference } => {
                anyhow::ensure!(url == "https://github.com/mykytanikitenko/rudzio");
                anyhow::ensure!(matches!(reference, Some(GitRef::Tag(t)) if t == "v0.1.0"));
            }
            other => anyhow::bail!("expected Git location, got {other:?}"),
        }
        anyhow::ensure!(spec.features.iter().any(|f| f == "common"));
        anyhow::ensure!(spec.features.iter().any(|f| f == "runtime-tokio-multi-thread"));
        Ok(())
    }

    #[rudzio::test]
    fn version_only_declaration_produces_version_location() -> anyhow::Result<()> {
        let mut dep = rudzio_dep();
        dep.version_req = "0.1".to_owned();

        let members = vec![member("app", vec![dep])];
        let spec = collect_rudzio_spec(&members, &BTreeMap::new(), &ws_root())?;

        match &spec.location {
            RudzioLocation::Version(v) => anyhow::ensure!(v == "0.1"),
            other => anyhow::bail!("expected Version location, got {other:?}"),
        }
        Ok(())
    }

    #[rudzio::test]
    fn features_union_across_multiple_members() -> anyhow::Result<()> {
        let mut a_dep = rudzio_dep();
        a_dep.version_req = "0.1".to_owned();
        a_dep.features = vec!["common".to_owned(), "runtime-tokio-multi-thread".to_owned()];

        let mut b_dep = rudzio_dep();
        b_dep.version_req = "0.1".to_owned();
        b_dep.features = vec!["common".to_owned(), "runtime-compio".to_owned()];

        let members = vec![member("a", vec![a_dep]), member("b", vec![b_dep])];
        let spec = collect_rudzio_spec(&members, &BTreeMap::new(), &ws_root())?;

        anyhow::ensure!(spec.features.contains(&"common".to_owned()));
        anyhow::ensure!(spec.features.contains(&"runtime-tokio-multi-thread".to_owned()));
        anyhow::ensure!(spec.features.contains(&"runtime-compio".to_owned()));
        Ok(())
    }

    #[rudzio::test]
    fn path_and_git_across_members_is_rejected() -> anyhow::Result<()> {
        let mut path_dep = rudzio_dep();
        path_dep.path = Some(PathBuf::from("/tmp/rudzio-checkout"));

        let mut git_dep = rudzio_dep();
        git_dep.git = Some("https://example.com/rudzio.git".to_owned());

        let members = vec![
            member("via_path", vec![path_dep]),
            member("via_git", vec![git_dep]),
        ];

        let err = collect_rudzio_spec(&members, &BTreeMap::new(), &ws_root())
            .expect_err("path+git across members must bail");

        let msg = err.to_string();
        anyhow::ensure!(msg.contains("inconsistently"), "msg: {msg}");
        anyhow::ensure!(msg.contains("via_path"), "msg: {msg}");
        anyhow::ensure!(msg.contains("via_git"), "msg: {msg}");
        Ok(())
    }

    #[rudzio::test]
    fn empty_members_falls_back_to_workspace_root_path() -> anyhow::Result<()> {
        let spec =
            collect_rudzio_spec(&[], &BTreeMap::new(), Path::new("/tmp/fallback"))?;

        match &spec.location {
            RudzioLocation::Path(p) => anyhow::ensure!(p == Path::new("/tmp/fallback")),
            other => anyhow::bail!("expected fallback Path, got {other:?}"),
        }
        Ok(())
    }

    #[rudzio::test]
    fn missing_workspace_entry_bails_with_member_name() -> anyhow::Result<()> {
        let mut dep = rudzio_dep();
        dep.workspace_inherited = true;

        let members = vec![member("orphan", vec![dep])];

        let err = collect_rudzio_spec(&members, &BTreeMap::new(), &ws_root())
            .expect_err("workspace = true with no workspace entry must bail");

        anyhow::ensure!(err.to_string().contains("orphan"), "msg: {err}");
        Ok(())
    }

    #[rudzio::test]
    fn emit_git_produces_git_and_rev_keys() -> anyhow::Result<()> {
        let spec = RudzioSpec {
            location: RudzioLocation::Git {
                url: "https://github.com/mykytanikitenko/rudzio".to_owned(),
                reference: Some(GitRef::Rev("abc123".to_owned())),
            },
            features: vec!["common".to_owned()],
            uses_default_features: true,
        };
        let rendered = build_rudzio_inline_table(&spec).to_string();

        anyhow::ensure!(rendered.contains("git = "), "rendered: {rendered}");
        anyhow::ensure!(
            rendered.contains("https://github.com/mykytanikitenko/rudzio"),
            "rendered: {rendered}",
        );
        anyhow::ensure!(rendered.contains("rev = \"abc123\""), "rendered: {rendered}");
        anyhow::ensure!(!rendered.contains("path ="), "rendered: {rendered}");
        anyhow::ensure!(!rendered.contains("version ="), "rendered: {rendered}");
        Ok(())
    }

    #[rudzio::test]
    fn emit_version_produces_version_key() -> anyhow::Result<()> {
        let spec = RudzioSpec {
            location: RudzioLocation::Version("0.1".to_owned()),
            features: Vec::new(),
            uses_default_features: true,
        };
        let rendered = build_rudzio_inline_table(&spec).to_string();

        anyhow::ensure!(rendered.contains("version = \"0.1\""), "rendered: {rendered}");
        anyhow::ensure!(!rendered.contains("git ="), "rendered: {rendered}");
        anyhow::ensure!(!rendered.contains("path ="), "rendered: {rendered}");
        Ok(())
    }

    #[rudzio::test]
    fn emit_default_features_false_appears_only_when_disabled() -> anyhow::Result<()> {
        let enabled = RudzioSpec {
            location: RudzioLocation::Version("0.1".to_owned()),
            features: Vec::new(),
            uses_default_features: true,
        };
        let rendered_enabled = build_rudzio_inline_table(&enabled).to_string();
        anyhow::ensure!(
            !rendered_enabled.contains("default-features"),
            "default-features should NOT appear when true: {rendered_enabled}",
        );

        let disabled = RudzioSpec {
            location: RudzioLocation::Version("0.1".to_owned()),
            features: Vec::new(),
            uses_default_features: false,
        };
        let rendered_disabled = build_rudzio_inline_table(&disabled).to_string();
        anyhow::ensure!(
            rendered_disabled.contains("default-features = false"),
            "default-features = false missing: {rendered_disabled}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn main_rs_emits_extern_crate_per_lib_member_for_rlib_linking() -> anyhow::Result<()> {
        let members = vec![
            member("alpha", Vec::new()),
            member("beta", Vec::new()),
        ];
        let rendered = build_main_rs(&plan_with_members(members, "/nowhere"));

        anyhow::ensure!(
            rendered.contains("extern crate alpha;"),
            "missing extern crate alpha; in:\n{rendered}",
        );
        anyhow::ensure!(
            rendered.contains("extern crate beta;"),
            "missing extern crate beta; in:\n{rendered}",
        );
        anyhow::ensure!(
            rendered.contains("#[rudzio::main]"),
            "missing #[rudzio::main] in:\n{rendered}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn main_rs_normalises_hyphens_to_underscores_in_extern_crate() -> anyhow::Result<()> {
        let members = vec![member("foo-bar-baz", Vec::new())];
        let rendered = build_main_rs(&plan_with_members(members, "/nowhere"));

        anyhow::ensure!(
            rendered.contains("extern crate foo_bar_baz;"),
            "hyphen-in-pkg-name not normalised to underscore:\n{rendered}",
        );
        anyhow::ensure!(
            !rendered.contains("extern crate foo-bar-baz;"),
            "emitted invalid ident with hyphens:\n{rendered}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn main_rs_skips_workspace_root_and_bin_only_members() -> anyhow::Result<()> {
        let mut root_member = member("root_crate", Vec::new());
        root_member.manifest_dir = PathBuf::from("/ws-root");

        let mut bin_only = member("only_bins", Vec::new());
        bin_only.has_lib = false;
        bin_only.bin_names = vec!["only_bins".to_owned()];

        let regular = member("regular", Vec::new());

        let members = vec![root_member, bin_only, regular];
        let rendered = build_main_rs(&plan_with_members(members, "/ws-root"));

        anyhow::ensure!(
            !rendered.contains("extern crate root_crate;"),
            "workspace-root member must not appear (already covered by rudzio dep):\n{rendered}",
        );
        anyhow::ensure!(
            !rendered.contains("extern crate only_bins;"),
            "bin-only member has no rlib to link:\n{rendered}",
        );
        anyhow::ensure!(
            rendered.contains("extern crate regular;"),
            "regular lib member is missing:\n{rendered}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn main_rs_dedups_extern_crate_on_name_collision() -> anyhow::Result<()> {
        // Two members whose normalised idents collide (cargo disallows
        // this in a real workspace, but the dedup keeps the emitted file
        // from containing a duplicate `extern crate foo_bar;`).
        let members = vec![
            member("foo-bar", Vec::new()),
            member("foo_bar", Vec::new()),
        ];
        let rendered = build_main_rs(&plan_with_members(members, "/nowhere"));

        let occurrences: Vec<&str> =
            rendered.match_indices("extern crate foo_bar;").map(|(_, s)| s).collect();
        anyhow::ensure!(
            occurrences.len() == 1,
            "expected one extern crate foo_bar;, got {}:\n{rendered}",
            occurrences.len(),
        );
        Ok(())
    }

    #[rudzio::test]
    fn emit_git_omits_reference_when_default_branch() -> anyhow::Result<()> {
        let spec = RudzioSpec {
            location: RudzioLocation::Git {
                url: "https://example.com/r".to_owned(),
                reference: None,
            },
            features: Vec::new(),
            uses_default_features: true,
        };
        let rendered = build_rudzio_inline_table(&spec).to_string();

        anyhow::ensure!(rendered.contains("git ="), "rendered: {rendered}");
        anyhow::ensure!(!rendered.contains("rev ="), "rendered: {rendered}");
        anyhow::ensure!(!rendered.contains("branch ="), "rendered: {rendered}");
        anyhow::ensure!(!rendered.contains("tag ="), "rendered: {rendered}");
        Ok(())
    }
}
