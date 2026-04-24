use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{
    DevDepSpec, GitRef, MemberPlan, Plan, RudzioLocation, RudzioSpec, WorkspaceDepSpec,
    bridge_applies_to, bridge_dir_name, bridge_package_name, build_bridge_build_rs,
    build_bridge_cargo_toml, build_main_rs, build_rudzio_inline_table, collect_rudzio_spec,
    detect_src_rudzio_suite, scan_unbroadened_cfg_test_mods,
    scan_unbroadened_cfg_test_mods_in_plan, write_bridge_crate, write_runner,
};

fn member(name: &str, dev_deps: Vec<DevDepSpec>) -> MemberPlan {
    MemberPlan {
        package_name: name.to_owned(),
        manifest_dir: PathBuf::from("/nonexistent"),
        test_files: Vec::new(),
        bin_names: Vec::new(),
        has_lib: true,
        dev_deps,
        edition: "2024".to_owned(),
        src_lib_path: Some(PathBuf::from("/nonexistent/src/lib.rs")),
        build_rs: None,
        build_deps: Vec::new(),
        has_src_rudzio_suite: false,
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
        BTreeMap, DevDepSpec, GitRef, Path, RudzioLocation, RudzioSpec, WorkspaceDepSpec,
        bridge_applies_to, bridge_dir_name, bridge_package_name, build_bridge_build_rs,
        build_bridge_cargo_toml, build_main_rs, build_rudzio_inline_table, collect_rudzio_spec,
        detect_src_rudzio_suite, member, plan_with_members, rudzio_dep,
        scan_unbroadened_cfg_test_mods, scan_unbroadened_cfg_test_mods_in_plan,
        write_bridge_crate, write_runner, ws_root,
    };
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

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

    #[rudzio::test]
    fn detect_src_rudzio_suite_finds_suite_attr() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let src = tmp.path().join("src");
        fs::create_dir_all(&src)?;
        fs::write(
            src.join("lib.rs"),
            "pub fn f() {}\n#[rudzio::suite([(runtime = x, suite = y, test = z)])]\nmod t {}\n",
        )?;

        anyhow::ensure!(detect_src_rudzio_suite(&src));
        Ok(())
    }

    #[rudzio::test]
    fn detect_src_rudzio_suite_finds_rudzio_test_cfg_marker() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let src = tmp.path().join("src");
        fs::create_dir_all(&src)?;
        fs::write(
            src.join("lib.rs"),
            "#[cfg(any(test, rudzio_test))]\nmod tests {}\n",
        )?;

        anyhow::ensure!(detect_src_rudzio_suite(&src));
        Ok(())
    }

    #[rudzio::test]
    fn detect_src_rudzio_suite_recurses_into_subdirectories() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let nested = tmp.path().join("src/foo/bar");
        fs::create_dir_all(&nested)?;
        fs::write(tmp.path().join("src/lib.rs"), "pub mod foo;\n")?;
        fs::write(tmp.path().join("src/foo/mod.rs"), "pub mod bar;\n")?;
        fs::write(
            nested.join("inner.rs"),
            "#[rudzio::suite([(runtime = x, suite = y, test = z)])]\nmod t {}\n",
        )?;

        anyhow::ensure!(detect_src_rudzio_suite(&tmp.path().join("src")));
        Ok(())
    }

    #[rudzio::test]
    fn detect_src_rudzio_suite_returns_false_when_no_markers() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let src = tmp.path().join("src");
        fs::create_dir_all(&src)?;
        fs::write(
            src.join("lib.rs"),
            "pub fn nothing_to_see() {}\n#[cfg(test)]\nmod tests {}\n",
        )?;

        anyhow::ensure!(!detect_src_rudzio_suite(&src));
        Ok(())
    }

    #[rudzio::test]
    fn detect_src_rudzio_suite_returns_false_when_src_missing() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        anyhow::ensure!(!detect_src_rudzio_suite(&tmp.path().join("nonexistent")));
        Ok(())
    }

    #[rudzio::test]
    fn bridge_applies_to_requires_has_lib_and_src_suite() -> anyhow::Result<()> {
        let base = member("foo", Vec::new());
        anyhow::ensure!(!bridge_applies_to(&base), "default member must not bridge");

        let mut with_suite = member("foo", Vec::new());
        with_suite.has_src_rudzio_suite = true;
        anyhow::ensure!(
            bridge_applies_to(&with_suite),
            "lib + src suite should bridge"
        );

        let mut bin_only = member("foo", Vec::new());
        bin_only.has_lib = false;
        bin_only.src_lib_path = None;
        bin_only.has_src_rudzio_suite = true;
        anyhow::ensure!(
            !bridge_applies_to(&bin_only),
            "bin-only member can't bridge even if src had markers"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_package_name_appends_suffix_and_normalises_hyphens() -> anyhow::Result<()> {
        let m = member("foo-bar", Vec::new());
        anyhow::ensure!(bridge_package_name(&m) == "foo_bar_rudzio_bridge");
        anyhow::ensure!(bridge_dir_name(&m) == "foo_bar");
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_emits_package_lib_and_deps() -> anyhow::Result<()> {
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.edition = "2021".to_owned();

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("name = \"alpha_rudzio_bridge\""),
            "missing bridge package name:\n{rendered}"
        );
        anyhow::ensure!(
            rendered.contains("edition = \"2021\""),
            "edition must be copied from member:\n{rendered}"
        );
        anyhow::ensure!(
            rendered.contains("name = \"alpha\"")
                && rendered.contains("path = \"/abs/alpha/src/lib.rs\""),
            "missing [lib] name/path pointing at real src/lib.rs:\n{rendered}"
        );
        anyhow::ensure!(
            !rendered.contains("[workspace]"),
            "bridge must NOT declare a [workspace] stanza (attaches to aggregator's):\n{rendered}"
        );
        anyhow::ensure!(
            rendered.contains("[dependencies]"),
            "bridge must emit [dependencies]:\n{rendered}"
        );
        anyhow::ensure!(
            rendered.contains("rudzio"),
            "rudzio must be in bridge deps:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_forwards_build_rs_when_member_has_no_bins() -> anyhow::Result<()> {
        // Preserve member's build.rs side effects (codegen, env-vars) when
        // there are no bins to expose: the bridge forwards the absolute
        // path and relies on the sentinel env var to short-circuit any
        // expose_bins calls inside.
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.bin_names = Vec::new();
        m.build_rs = Some(PathBuf::from("/abs/alpha/build.rs"));

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("build = \"/abs/alpha/build.rs\""),
            "build.rs path must be forwarded when member has no bins:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_synthesizes_build_rs_when_member_has_bins() -> anyhow::Result<()> {
        // When a member declares bins, the bridge CANNOT forward the
        // member's build.rs because any `rudzio::build::expose_self_bins()`
        // call inside it queries CARGO_PKG_NAME (= bridge's name) which
        // has no `[[bin]]` targets. Instead the bridge synthesises its
        // own build.rs that calls expose_member_bins against the real
        // member package, and references it as a LOCAL `build.rs` file.
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.bin_names = vec!["alpha-server".to_owned()];
        m.build_rs = None;

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("build = \"build.rs\""),
            "bridge must reference a local build.rs when member has bins:\n{rendered}"
        );
        anyhow::ensure!(
            !rendered.contains("build = \"/abs/alpha/build.rs\""),
            "bridge must NOT forward the member's build.rs absolute path when it synthesises its own:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_synthesized_build_rs_replaces_forward_when_member_has_both() -> anyhow::Result<()> {
        // Priority: if member has bins, the synthesised build.rs wins —
        // the member's own build.rs (if any) is NOT forwarded. This is
        // the file-v3 shape: expose_self_bins in the member's build.rs
        // does not work under the bridge (no [[bin]] on bridge package),
        // so we replace with the generated expose_member_bins call.
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.bin_names = vec!["alpha-server".to_owned()];
        m.build_rs = Some(PathBuf::from("/abs/alpha/build.rs"));

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("build = \"build.rs\""),
            "bridge must synthesise a local build.rs when member has bins, not forward:\n{rendered}"
        );
        anyhow::ensure!(
            !rendered.contains("/abs/alpha/build.rs"),
            "bridge must NOT reference the member's absolute build.rs path:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_synthesized_build_rs_calls_expose_member_bins_per_bin() -> anyhow::Result<()> {
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.bin_names = vec!["alpha-server".to_owned(), "alpha-cli".to_owned()];

        let content = build_bridge_build_rs(&m)
            .expect("bridge must synthesise build.rs content for bin members");

        anyhow::ensure!(
            content.contains("fn main()"),
            "synthesised build.rs needs fn main:\n{content}"
        );
        anyhow::ensure!(
            content.contains("expose_member_bins"),
            "synthesised build.rs must call expose_member_bins helper:\n{content}"
        );
        anyhow::ensure!(
            content.contains("\"alpha\"") && content.contains("\"/abs/alpha\""),
            "synthesised build.rs must pass member pkg name + manifest dir:\n{content}"
        );
        anyhow::ensure!(
            content.contains("\"alpha-server\"") && content.contains("\"alpha-cli\""),
            "synthesised build.rs must list every bin:\n{content}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_synthesized_build_rs_none_when_no_bins() -> anyhow::Result<()> {
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.bin_names = Vec::new();

        anyhow::ensure!(
            build_bridge_build_rs(&m).is_none(),
            "no synthesised build.rs when member has no bins"
        );
        Ok(())
    }

    #[rudzio::test]
    fn write_bridge_crate_writes_synthesized_build_rs_file() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.bin_names = vec!["alpha-server".to_owned()];

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let out_dir = tmp.path().join("members");
        fs::create_dir_all(&out_dir)?;
        write_bridge_crate(&plan, &m, &out_dir)?;

        let bridge_build = out_dir.join(bridge_dir_name(&m)).join("build.rs");
        anyhow::ensure!(
            bridge_build.exists(),
            "bridge build.rs file must be written to {}",
            bridge_build.display()
        );
        let content = fs::read_to_string(&bridge_build)?;
        anyhow::ensure!(
            content.contains("expose_member_bins(\"alpha\"")
                && content.contains("\"alpha-server\""),
            "bridge build.rs missing expected content:\n{content}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_omits_build_when_absent() -> anyhow::Result<()> {
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.build_rs = None;

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            !rendered.contains("build = "),
            "build must be absent when member has no build.rs:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_emits_build_dependencies_when_member_has_them() -> anyhow::Result<()> {
        // Regression: the bridge forwards `build = "<abs>/build.rs"`, so
        // the bridge's compilation of that build script needs matching
        // `[build-dependencies]`. Without this, a member's build.rs that
        // imports e.g. `rudzio::build::expose_self_bins()` fails to resolve
        // under `cargo rudzio test` even though the member itself builds
        // fine under regime 2.
        let rudzio_build_dep = DevDepSpec {
            name: "rudzio".to_owned(),
            rename: None,
            version_req: "0.1".to_owned(),
            path: None,
            git: None,
            git_ref: None,
            features: Vec::new(),
            uses_default_features: true,
            workspace_inherited: false,
        };
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.build_rs = Some(PathBuf::from("/abs/alpha/build.rs"));
        m.build_deps = vec![rudzio_build_dep];

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("[build-dependencies]"),
            "bridge must emit [build-dependencies] when member has build-deps:\n{rendered}"
        );
        // The rudzio entry must appear AFTER the [build-dependencies]
        // header — a naive substring check would also match the main
        // [dependencies] table, so split on the header and inspect the
        // tail.
        let (_, after) = rendered
            .split_once("[build-dependencies]")
            .expect("[build-dependencies] header present");
        anyhow::ensure!(
            after.contains("rudzio"),
            "rudzio must be inside the [build-dependencies] table:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_omits_build_dependencies_when_member_has_none() -> anyhow::Result<()> {
        // Negative: a member with build.rs but no [build-dependencies] must
        // NOT get a synthesised `[build-dependencies]` section.
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.build_rs = Some(PathBuf::from("/abs/alpha/build.rs"));
        m.build_deps = Vec::new();

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            !rendered.contains("[build-dependencies]"),
            "no [build-dependencies] when member has none:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_expands_workspace_inherited_git_build_dep() -> anyhow::Result<()> {
        // Regression: a member declaring `rudzio = { workspace = true }`
        // in [build-dependencies] when the workspace entry is pinned with
        // only `git = "..." rev = "..."` (no path or version) previously
        // bailed in render_dev_dep's workspace_inherited branch. The
        // branch must also accept git-only workspace entries and emit
        // `git = "..." rev/branch/tag = "..."` into the bridge.
        let inherited_rudzio = DevDepSpec {
            name: "rudzio".to_owned(),
            rename: None,
            version_req: String::new(),
            path: None,
            git: None,
            git_ref: None,
            features: Vec::new(),
            uses_default_features: true,
            workspace_inherited: true,
        };
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.build_rs = Some(PathBuf::from("/abs/alpha/build.rs"));
        m.build_deps = vec![inherited_rudzio];

        let mut plan = plan_with_members(vec![m.clone()], "/abs");
        drop(plan.workspace_deps.insert(
            "rudzio".to_owned(),
            WorkspaceDepSpec {
                version_req: None,
                path: None,
                git: Some("https://github.com/mykytanikitenko/rudzio".to_owned()),
                git_ref: Some(GitRef::Rev("9422cd4590765a9cfc97774d638429efc8239d48".to_owned())),
                features: Vec::new(),
                uses_default_features: true,
            },
        ));
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        let (_, after) = rendered
            .split_once("[build-dependencies]")
            .expect("[build-dependencies] present");
        anyhow::ensure!(
            after.contains("git = \"https://github.com/mykytanikitenko/rudzio\""),
            "build-dep must carry the workspace git URL:\n{rendered}"
        );
        anyhow::ensure!(
            after.contains("rev = \"9422cd4590765a9cfc97774d638429efc8239d48\""),
            "build-dep must carry the workspace rev:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_omits_build_dependencies_when_no_build_rs() -> anyhow::Result<()> {
        // Negative: even if build_deps accidentally contains entries (e.g.
        // a stale plan), without build_rs there's nothing to compile; skip
        // the section.
        let stray = DevDepSpec {
            name: "stray".to_owned(),
            rename: None,
            version_req: "1".to_owned(),
            path: None,
            git: None,
            git_ref: None,
            features: Vec::new(),
            uses_default_features: true,
            workspace_inherited: false,
        };
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.build_rs = None;
        m.build_deps = vec![stray];

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            !rendered.contains("[build-dependencies]"),
            "no [build-dependencies] when member has no build.rs:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_does_not_push_anyhow_onto_users() -> anyhow::Result<()> {
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            !rendered.contains("\nanyhow")
                && !rendered.contains(" anyhow ")
                && !rendered.contains("\"anyhow\""),
            "bridge must NOT inject anyhow — rudzio's void-fn rewrite uses rudzio::BoxError, no user-side anyhow dep needed:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_preserves_existing_member_deps() -> anyhow::Result<()> {
        let mut custom = DevDepSpec {
            name: "serde_json".to_owned(),
            rename: None,
            version_req: "1".to_owned(),
            path: None,
            git: None,
            git_ref: None,
            features: Vec::new(),
            uses_default_features: true,
            workspace_inherited: false,
        };
        custom.version_req = "1.0".to_owned();

        let mut m = member("alpha", vec![rudzio_dep(), custom]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("serde_json"),
            "member's existing dep must survive into bridge:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridges_emitted_without_any_opt_in() -> anyhow::Result<()> {
        // A.1: bridges are unconditional for qualifying members. No
        // `plan.generate_bridges = true` flag — because the field no
        // longer exists.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m], "/abs");

        let out = tempdir()?;
        write_runner(&plan, out.path())?;

        let bridge_manifest = out.path().join("members/alpha/Cargo.toml");
        anyhow::ensure!(
            bridge_manifest.exists(),
            "bridge Cargo.toml must be written under members/<name>/ unconditionally"
        );

        let aggregator_manifest = fs::read_to_string(out.path().join("Cargo.toml"))?;
        anyhow::ensure!(
            aggregator_manifest.contains("package = \"alpha_rudzio_bridge\""),
            "aggregator must reference bridge by package rename:\n{aggregator_manifest}"
        );
        anyhow::ensure!(
            aggregator_manifest.contains("path = \"./members/alpha\""),
            "aggregator path must point at bridge dir:\n{aggregator_manifest}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn members_without_src_suite_keep_direct_path_dep() -> anyhow::Result<()> {
        // A.2: the `has_src_rudzio_suite` gate still decides per-member.
        // Bridge for members whose src has rudzio suites; direct path
        // dep for the rest.
        let mut bridged = member("with_src", vec![rudzio_dep()]);
        bridged.manifest_dir = PathBuf::from("/abs/with_src");
        bridged.src_lib_path = Some(PathBuf::from("/abs/with_src/src/lib.rs"));
        bridged.has_src_rudzio_suite = true;

        let mut tests_only = member("tests_only", vec![rudzio_dep()]);
        tests_only.manifest_dir = PathBuf::from("/abs/tests_only");
        tests_only.src_lib_path = Some(PathBuf::from("/abs/tests_only/src/lib.rs"));
        tests_only.has_src_rudzio_suite = false;

        let plan = plan_with_members(vec![bridged, tests_only], "/abs");

        let out = tempdir()?;
        write_runner(&plan, out.path())?;

        anyhow::ensure!(
            out.path().join("members/with_src/Cargo.toml").exists(),
            "src-suite member must get a bridge dir"
        );
        anyhow::ensure!(
            !out.path().join("members/tests_only/Cargo.toml").exists(),
            "member with no src suite must NOT get a bridge"
        );

        let aggregator_manifest = fs::read_to_string(out.path().join("Cargo.toml"))?;
        anyhow::ensure!(
            aggregator_manifest.contains("package = \"with_src_rudzio_bridge\""),
            "bridged member must be referenced by package rename:\n{aggregator_manifest}"
        );
        anyhow::ensure!(
            aggregator_manifest.contains("path = \"/abs/tests_only\""),
            "non-bridged member keeps direct path dep:\n{aggregator_manifest}"
        );
        anyhow::ensure!(
            !aggregator_manifest.contains("tests_only_rudzio_bridge"),
            "non-bridged member must NOT get a bridge package rename:\n{aggregator_manifest}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bin_only_member_is_never_bridged() -> anyhow::Result<()> {
        // A.3: bin-only members cannot be bridged even if
        // has_src_rudzio_suite is set (contrived — bridges re-point
        // [lib] path, which requires a lib target). Verified via the
        // pure gate predicate `bridge_applies_to`.
        let mut m = member("bin_only", Vec::new());
        m.has_lib = false;
        m.src_lib_path = None;
        m.has_src_rudzio_suite = true; // contrived, should still not bridge
        m.bin_names = vec!["bin_only".to_owned()];

        anyhow::ensure!(
            !bridge_applies_to(&m),
            "bin-only member must be rejected by bridge_applies_to regardless of has_src_rudzio_suite"
        );

        // And end-to-end: write_runner does not emit a bridge dir.
        let plan = plan_with_members(vec![m], "/abs");
        let out = tempdir()?;
        write_runner(&plan, out.path())?;
        anyhow::ensure!(
            !out.path().join("members/bin_only/Cargo.toml").exists(),
            "bin-only member must not produce a bridge dir"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridged_members_listed_in_aggregator_workspace_members() -> anyhow::Result<()> {
        // A.4: aggregator's `[workspace] members = [...]` must list
        // each bridged member. Regression guard: without this, cargo
        // rejects with "multiple workspace roots found" because the
        // bridge Cargo.tomls are nested under the aggregator's dir.
        let mut a = member("alpha", vec![rudzio_dep()]);
        a.manifest_dir = PathBuf::from("/abs/alpha");
        a.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        a.has_src_rudzio_suite = true;

        let mut b = member("beta", vec![rudzio_dep()]);
        b.manifest_dir = PathBuf::from("/abs/beta");
        b.src_lib_path = Some(PathBuf::from("/abs/beta/src/lib.rs"));
        b.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![a, b], "/abs");
        let out = tempdir()?;
        write_runner(&plan, out.path())?;

        let manifest = fs::read_to_string(out.path().join("Cargo.toml"))?;
        anyhow::ensure!(
            manifest.contains("members/alpha") && manifest.contains("members/beta"),
            "aggregator [workspace] members must list every bridge dir:\n{manifest}"
        );
        // Sanity: the listing is inside a [workspace] stanza.
        let ws_start = manifest
            .find("[workspace]")
            .ok_or_else(|| anyhow::anyhow!("no [workspace] in:\n{manifest}"))?;
        let members_pos = manifest.find("members/alpha").unwrap_or(0);
        anyhow::ensure!(
            members_pos > ws_start,
            "members list must appear under [workspace], not elsewhere"
        );
        Ok(())
    }

    // ── B. Diagnostic warnings for unbroadened #[cfg(test)] mods ───────

    fn mk_member_with_src(tmp: &::std::path::Path, name: &str, files: &[(&str, &str)]) -> super::MemberPlan {
        let manifest_dir = tmp.join(name);
        let src_dir = manifest_dir.join("src");
        fs::create_dir_all(&src_dir).expect("mkdir src");
        fs::write(src_dir.join("lib.rs"), "// placeholder\n").expect("write lib.rs");
        for (rel, content) in files {
            let p = src_dir.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("mkdir intermediate");
            }
            fs::write(&p, content).expect("write src file");
        }
        let mut m = member(name, Vec::new());
        m.manifest_dir = manifest_dir.clone();
        m.src_lib_path = Some(src_dir.join("lib.rs"));
        m.has_lib = true;
        m.has_src_rudzio_suite = false;
        m
    }

    #[rudzio::test]
    fn warns_on_bare_cfg_test_mod_without_rudzio() -> anyhow::Result<()> {
        // B.1: plain `#[cfg(test)] mod tests` with no rudzio markers →
        // one warning naming the file:line.
        let tmp = tempdir()?;
        let m = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[(
                "lib.rs",
                "pub fn f() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn x() {}\n}\n",
            )],
        );
        let warnings = scan_unbroadened_cfg_test_mods(&m);
        anyhow::ensure!(
            warnings.len() == 1,
            "expected exactly one warning, got {}: {:?}",
            warnings.len(),
            warnings
        );
        let w = &warnings[0];
        anyhow::ensure!(
            w.contains("lib.rs:3") || w.contains("lib.rs:3:"),
            "warning must cite file:line for the cfg(test) attr on mod tests: {w}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn no_warning_when_already_broadened() -> anyhow::Result<()> {
        // B.2: `#[cfg(any(test, rudzio_test))]` → zero warnings.
        let tmp = tempdir()?;
        let m = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[(
                "lib.rs",
                "#[cfg(any(test, rudzio_test))]\nmod tests {}\n",
            )],
        );
        let warnings = scan_unbroadened_cfg_test_mods(&m);
        anyhow::ensure!(
            warnings.is_empty(),
            "broadened gate must produce no warnings, got: {warnings:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn no_warning_when_rudzio_suite_substring_present() -> anyhow::Result<()> {
        // B.3: if `rudzio::suite` appears anywhere in the file, the
        // user is in charge — suppress the warning.
        let tmp = tempdir()?;
        let m = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[(
                "lib.rs",
                "#[cfg(test)]\n#[::rudzio::suite([( runtime = _, suite = _, test = _)])]\nmod tests {}\n",
            )],
        );
        let warnings = scan_unbroadened_cfg_test_mods(&m);
        anyhow::ensure!(
            warnings.is_empty(),
            "rudzio::suite in file must suppress cfg(test) warning: {warnings:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn no_warning_when_rudzio_test_substring_present() -> anyhow::Result<()> {
        // B.4: `rudzio_test` anywhere in the file suppresses the
        // warning — the user knows about the cfg.
        let tmp = tempdir()?;
        let m = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[(
                "lib.rs",
                "// rudzio_test opt-in here\n#[cfg(test)]\nmod tests {}\n",
            )],
        );
        let warnings = scan_unbroadened_cfg_test_mods(&m);
        anyhow::ensure!(
            warnings.is_empty(),
            "`rudzio_test` substring must suppress warning: {warnings:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn no_warning_when_file_has_no_tests() -> anyhow::Result<()> {
        // B.5: no cfg(test) at all → no warnings.
        let tmp = tempdir()?;
        let m = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[("lib.rs", "pub fn hello() -> u8 { 1 }\n")],
        );
        let warnings = scan_unbroadened_cfg_test_mods(&m);
        anyhow::ensure!(
            warnings.is_empty(),
            "plain source must not trigger warnings: {warnings:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn multiple_violations_collected_per_file() -> anyhow::Result<()> {
        // B.6: every violating site warns independently.
        let tmp = tempdir()?;
        let m = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[(
                "lib.rs",
                "#[cfg(test)]\nmod a {}\n\n#[cfg(test)]\nmod b {}\n",
            )],
        );
        let warnings = scan_unbroadened_cfg_test_mods(&m);
        anyhow::ensure!(
            warnings.len() == 2,
            "expected 2 warnings for 2 bare cfg(test) mods, got {}: {:?}",
            warnings.len(),
            warnings
        );
        Ok(())
    }

    #[rudzio::test]
    fn warning_message_includes_actionable_fix() -> anyhow::Result<()> {
        // B.7: the warning must literally contain the replacement
        // snippet so a user can copy-paste.
        let tmp = tempdir()?;
        let m = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[(
                "lib.rs",
                "#[cfg(test)]\nmod tests {}\n",
            )],
        );
        let warnings = scan_unbroadened_cfg_test_mods(&m);
        anyhow::ensure!(
            warnings.iter().any(|w| w.contains("#[cfg(any(test, rudzio_test))]")),
            "warning must suggest the broadened gate verbatim: {warnings:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn warnings_aggregate_across_members() -> anyhow::Result<()> {
        // B.8: a plan-level scan returns the union across members in
        // deterministic order.
        let tmp = tempdir()?;
        let a = mk_member_with_src(
            tmp.path(),
            "alpha",
            &[("lib.rs", "#[cfg(test)]\nmod tests {}\n")],
        );
        let b = mk_member_with_src(
            tmp.path(),
            "beta",
            &[("lib.rs", "#[cfg(test)]\nmod tests {}\n")],
        );
        let plan = plan_with_members(vec![a, b], tmp.path().to_string_lossy().as_ref());
        let all = scan_unbroadened_cfg_test_mods_in_plan(&plan);
        anyhow::ensure!(
            all.len() == 2,
            "expected one warning per member (2 total), got {}: {:?}",
            all.len(),
            all
        );
        // Deterministic order: alpha before beta (members are sorted
        // by package name in build_plan).
        let idx_alpha = all.iter().position(|w| w.contains("alpha"));
        let idx_beta = all.iter().position(|w| w.contains("beta"));
        anyhow::ensure!(
            matches!((idx_alpha, idx_beta), (Some(a), Some(b)) if a < b),
            "alpha's warning must precede beta's: {all:?}"
        );
        Ok(())
    }
}
