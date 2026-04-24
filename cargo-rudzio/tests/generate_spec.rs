use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{
    DevDepSpec, GitRef, MemberPlan, Plan, RudzioLocation, RudzioSpec, WorkspaceDepSpec,
    bridge_applies_to, bridge_dir_name, bridge_package_name, build_bridge_build_rs,
    build_bridge_cargo_toml, build_main_rs, build_rudzio_inline_table, collect_rudzio_spec,
    detect_src_rudzio_suite, load_rudzio_activated_features, scan_unbroadened_cfg_test_mods,
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
        has_src_rudzio_suite: false,
        features: BTreeMap::new(),
        rudzio_activated_features: Vec::new(),
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
        optional: false,
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
        detect_src_rudzio_suite, load_rudzio_activated_features, member, plan_with_members,
        rudzio_dep, scan_unbroadened_cfg_test_mods, scan_unbroadened_cfg_test_mods_in_plan,
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
                && rendered.contains("path = \"src/lib.rs\""),
            "missing [lib] name/path (should be relative, resolved via symlink):\n{rendered}"
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
    fn bridge_cargo_toml_always_references_local_build_rs() -> anyhow::Result<()> {
        // Every bridge synthesises its own build.rs (emits
        // `cargo:rustc-cfg=rudzio_test` for the bridge compile unit, plus
        // `expose_member_bins` for bin members). Cargo.toml therefore
        // always references a local `build.rs`, never a forwarded path.
        for bins in [Vec::new(), vec!["alpha-server".to_owned()]] {
            let mut m = member("alpha", Vec::new());
            m.manifest_dir = PathBuf::from("/abs/alpha");
            m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
            m.has_src_rudzio_suite = true;
            m.bin_names = bins.clone();

            let plan = plan_with_members(vec![m.clone()], "/abs");
            let rendered = build_bridge_cargo_toml(&plan, &m)?;

            anyhow::ensure!(
                rendered.contains("build = \"build.rs\""),
                "bridge must always reference local build.rs (bins={bins:?}):\n{rendered}"
            );
        }
        Ok(())
    }

    #[rudzio::test]
    fn bridge_synthesized_build_rs_calls_expose_member_bins_per_bin() -> anyhow::Result<()> {
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.bin_names = vec!["alpha-server".to_owned(), "alpha-cli".to_owned()];

        let content = build_bridge_build_rs(&m);

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
    fn bridge_synthesized_build_rs_emits_rudzio_test_cfg_for_binless_member()
    -> anyhow::Result<()> {
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.bin_names = Vec::new();

        let content = build_bridge_build_rs(&m);
        anyhow::ensure!(
            content.contains("cargo:rustc-cfg=rudzio_test"),
            "bin-less bridge build.rs must emit the cfg:\n{content}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_synthesized_build_rs_emits_rudzio_test_cfg_for_bin_member()
    -> anyhow::Result<()> {
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.bin_names = vec!["alpha-server".to_owned()];

        let content = build_bridge_build_rs(&m);

        anyhow::ensure!(
            content.contains("cargo:rustc-cfg=rudzio_test"),
            "bridge build.rs must emit the rudzio_test cfg:\n{content}"
        );
        anyhow::ensure!(
            content.contains("expose_member_bins"),
            "bin-member bridge build.rs must still expose the bins:\n{content}"
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
    fn aggregator_build_rs_emits_rudzio_test_cfg_with_no_bin_members() -> anyhow::Result<()> {
        // Architectural fix: cargo-rudzio no longer transports the
        // `--cfg rudzio_test` flag via ambient RUSTFLAGS. Instead, the
        // aggregator's own build.rs emits `cargo:rustc-cfg=rudzio_test`
        // so the cfg is scoped to the aggregator compile unit only.
        // Even with zero bin members the build.rs must still emit it —
        // the aggregator's lib + tests are exactly what the cfg gates.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.bin_names = Vec::new();

        let plan = plan_with_members(vec![m], "/abs");
        let out = tempdir()?;
        write_runner(&plan, out.path())?;

        let aggregator_build_rs = out.path().join("build.rs");
        anyhow::ensure!(
            aggregator_build_rs.exists(),
            "aggregator must always have a build.rs (for cfg emit) at {}",
            aggregator_build_rs.display()
        );
        let content = fs::read_to_string(&aggregator_build_rs)?;
        anyhow::ensure!(
            content.contains("cargo:rustc-cfg=rudzio_test"),
            "aggregator build.rs must emit the rudzio_test cfg:\n{content}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn aggregator_build_rs_emits_rudzio_test_cfg_with_bin_members() -> anyhow::Result<()> {
        // Same invariant when bin members are present — the cfg emit
        // sits alongside the existing `expose_member_bins` calls.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.bin_names = vec!["alpha-server".to_owned()];

        let plan = plan_with_members(vec![m], "/abs");
        let out = tempdir()?;
        write_runner(&plan, out.path())?;

        let content = fs::read_to_string(out.path().join("build.rs"))?;
        anyhow::ensure!(
            content.contains("cargo:rustc-cfg=rudzio_test"),
            "aggregator build.rs must emit the rudzio_test cfg:\n{content}"
        );
        anyhow::ensure!(
            content.contains("expose_member_bins"),
            "aggregator build.rs must still expose member bins:\n{content}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn write_bridge_crate_writes_build_rs_for_binless_member() -> anyhow::Result<()> {
        // Every bridge — bin-less ones included — must get a build.rs
        // written so the cfg emission lands in the right compile unit.
        let tmp = tempdir()?;
        let mut m = member("alpha", Vec::new());
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.bin_names = Vec::new();

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let out_dir = tmp.path().join("members");
        fs::create_dir_all(&out_dir)?;
        write_bridge_crate(&plan, &m, &out_dir)?;

        let bridge_build = out_dir.join(bridge_dir_name(&m)).join("build.rs");
        anyhow::ensure!(
            bridge_build.exists(),
            "bin-less bridge must still get a build.rs at {}",
            bridge_build.display()
        );
        let content = fs::read_to_string(&bridge_build)?;
        anyhow::ensure!(
            content.contains("cargo:rustc-cfg=rudzio_test"),
            "bin-less bridge build.rs must emit the cfg:\n{content}"
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
            optional: false,
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

    // ── C. Bridge feature mirroring (Issue 7) ──────────────────────────

    #[rudzio::test]
    fn bridge_cargo_toml_omits_features_when_member_has_none() -> anyhow::Result<()> {
        // Empty features map → bridge Cargo.toml stays as before, no
        // [features] section. Back-compat for members without features.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            !rendered.contains("[features]"),
            "no [features] section expected when member has no features:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_mirrors_member_features_one_to_one() -> anyhow::Result<()> {
        // Member's [features] table is mirrored verbatim into the
        // bridge's [features] table so cfg(feature = "...") gates in
        // member src/lib.rs can evaluate against the same universe of
        // feature names they would under `cargo test` in the member
        // itself.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.features = BTreeMap::from([
            ("unit".to_owned(), Vec::new()),
            ("test".to_owned(), vec!["dep:mockall".to_owned()]),
            ("integration".to_owned(), vec!["unit".to_owned()]),
        ]);

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("[features]"),
            "[features] section expected when member has features:\n{rendered}"
        );
        anyhow::ensure!(
            rendered.contains("unit = []"),
            "empty-value feature must mirror as `unit = []`:\n{rendered}"
        );
        anyhow::ensure!(
            rendered.contains("\"dep:mockall\""),
            "dep:mockall value must appear:\n{rendered}"
        );
        anyhow::ensure!(
            rendered.contains("integration"),
            "integration feature key must appear:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_sets_default_to_member_default() -> anyhow::Result<()> {
        // Member's own `default = ["foo"]` survives into the bridge,
        // simulating what `cargo test` would activate by default.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.features = BTreeMap::from([
            ("default".to_owned(), vec!["foo".to_owned()]),
            ("foo".to_owned(), Vec::new()),
        ]);

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("default = [\"foo\"]"),
            "bridge default must carry member's default:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_sets_default_to_rudzio_activated_features() -> anyhow::Result<()> {
        // rudzio_activated_features lists features (e.g. "unit", "test")
        // that the member opts in to under `cargo rudzio test`. These
        // become the bridge's `default` so they fire without the
        // aggregator pipeline needing per-member --features flags.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.features = BTreeMap::from([
            ("unit".to_owned(), Vec::new()),
            ("test".to_owned(), Vec::new()),
        ]);
        m.rudzio_activated_features = vec!["unit".to_owned(), "test".to_owned()];

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("default = [\"test\", \"unit\"]")
                || rendered.contains("default = [\"unit\", \"test\"]"),
            "bridge default must contain opt-in features (order-insensitive):\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_cargo_toml_unions_member_default_and_rudzio_activated() -> anyhow::Result<()> {
        // Member's `default = ["foo"]` + rudzio opt-in `["unit"]` →
        // bridge default is the union `["foo", "unit"]`, deduped and
        // sorted for determinism.
        let mut m = member("alpha", vec![rudzio_dep()]);
        m.manifest_dir = PathBuf::from("/abs/alpha");
        m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
        m.has_src_rudzio_suite = true;
        m.features = BTreeMap::from([
            ("default".to_owned(), vec!["foo".to_owned()]),
            ("foo".to_owned(), Vec::new()),
            ("unit".to_owned(), Vec::new()),
        ]);
        m.rudzio_activated_features = vec!["unit".to_owned()];

        let plan = plan_with_members(vec![m.clone()], "/abs");
        let rendered = build_bridge_cargo_toml(&plan, &m)?;

        anyhow::ensure!(
            rendered.contains("default = [\"foo\", \"unit\"]"),
            "bridge default must be the deduped sorted union:\n{rendered}"
        );
        Ok(())
    }

    // ── D. Bridge co-location via symlinks (Issue 8) ───────────────────

    #[rudzio::test]
    fn write_bridge_crate_symlinks_member_src_dir() -> anyhow::Result<()> {
        // Path-based macros like include_str!("data.json") resolve via
        // CARGO_MANIFEST_DIR at compile time. Under bridge compile the
        // manifest dir is target/.../members/<name>/, not the member's
        // real dir — so unless the member's top-level entries are
        // reachable from the bridge dir, every such macro fails.
        // write_bridge_crate symlinks every non-skiplist entry so
        // bridge_dir/src → member/src resolves transparently.
        let tmp = tempdir()?;
        let member_root = tmp.path().join("member");
        let member_src = member_root.join("src");
        fs::create_dir_all(&member_src)?;
        fs::write(member_src.join("lib.rs"), "// member lib\n")?;
        fs::write(member_root.join("Cargo.toml"), "[package]\nname = \"alpha\"\n")?;

        let mut m = member("alpha", Vec::new());
        m.manifest_dir = member_root.clone();
        m.src_lib_path = Some(member_src.join("lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], tmp.path().to_string_lossy().as_ref());
        let out_dir = tmp.path().join("bridges");
        fs::create_dir_all(&out_dir)?;
        write_bridge_crate(&plan, &m, &out_dir)?;

        let bridge_src = out_dir.join(bridge_dir_name(&m)).join("src");
        let meta = fs::symlink_metadata(&bridge_src)?;
        anyhow::ensure!(
            meta.file_type().is_symlink(),
            "bridge_dir/src must be a symlink, was: {:?}",
            meta.file_type()
        );
        let target = fs::read_link(&bridge_src)?;
        anyhow::ensure!(
            target == member_src,
            "bridge_dir/src symlink must target member/src; target={}",
            target.display()
        );
        Ok(())
    }

    #[rudzio::test]
    fn write_bridge_crate_symlinks_migrations_dir() -> anyhow::Result<()> {
        // Any member top-level entry (not in the skiplist) gets
        // symlinked — including sidecar dirs like `migrations/` that
        // sqlx::migrate! resolves via CARGO_MANIFEST_DIR.
        let tmp = tempdir()?;
        let member_root = tmp.path().join("member");
        fs::create_dir_all(member_root.join("src"))?;
        fs::write(member_root.join("src").join("lib.rs"), "// lib\n")?;
        fs::create_dir_all(member_root.join("migrations"))?;
        fs::write(member_root.join("migrations").join("0001_init.sql"), "-- sql\n")?;
        fs::write(member_root.join("Cargo.toml"), "[package]\nname = \"alpha\"\n")?;

        let mut m = member("alpha", Vec::new());
        m.manifest_dir = member_root.clone();
        m.src_lib_path = Some(member_root.join("src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], tmp.path().to_string_lossy().as_ref());
        let out_dir = tmp.path().join("bridges");
        fs::create_dir_all(&out_dir)?;
        write_bridge_crate(&plan, &m, &out_dir)?;

        let bridge_migrations = out_dir.join(bridge_dir_name(&m)).join("migrations");
        anyhow::ensure!(
            fs::symlink_metadata(&bridge_migrations)?.file_type().is_symlink(),
            "bridge_dir/migrations must be a symlink"
        );
        Ok(())
    }

    #[rudzio::test]
    fn write_bridge_crate_skips_cargo_toml_build_rs_target_git() -> anyhow::Result<()> {
        // Skiplist: entries that either conflict with the bridge's own
        // synthesised files (Cargo.toml, build.rs) or would be actively
        // harmful (target/ recursion, .git/ confusing tooling). These
        // must NOT be symlinked.
        let tmp = tempdir()?;
        let member_root = tmp.path().join("member");
        fs::create_dir_all(member_root.join("src"))?;
        fs::write(member_root.join("src").join("lib.rs"), "// lib\n")?;
        fs::write(member_root.join("Cargo.toml"), "[package]\nname = \"alpha\"\n")?;
        fs::write(member_root.join("Cargo.lock"), "# lockfile\n")?;
        fs::write(member_root.join("build.rs"), "fn main() {}\n")?;
        fs::create_dir_all(member_root.join("target"))?;
        fs::create_dir_all(member_root.join(".git"))?;

        let mut m = member("alpha", Vec::new());
        m.manifest_dir = member_root.clone();
        m.src_lib_path = Some(member_root.join("src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], tmp.path().to_string_lossy().as_ref());
        let out_dir = tmp.path().join("bridges");
        fs::create_dir_all(&out_dir)?;
        write_bridge_crate(&plan, &m, &out_dir)?;

        let bridge_root = out_dir.join(bridge_dir_name(&m));

        // Cargo.toml and build.rs are written as real files, not symlinks.
        let cargo_toml_meta = fs::symlink_metadata(bridge_root.join("Cargo.toml"))?;
        anyhow::ensure!(
            !cargo_toml_meta.file_type().is_symlink(),
            "bridge Cargo.toml must be a real file (synthesised), not a symlink to member's"
        );
        let build_rs_meta = fs::symlink_metadata(bridge_root.join("build.rs"))?;
        anyhow::ensure!(
            !build_rs_meta.file_type().is_symlink(),
            "bridge build.rs must be a real file (synthesised), not a symlink to member's"
        );

        // Cargo.lock, target/, .git/ must not appear at all in the bridge.
        for skipped in &["Cargo.lock", "target", ".git"] {
            anyhow::ensure!(
                !bridge_root.join(skipped).exists(),
                "`{skipped}` must not appear in bridge dir (neither real nor symlinked)"
            );
        }
        Ok(())
    }

    #[rudzio::test]
    fn write_bridge_crate_bridge_cargo_toml_uses_relative_lib_path() -> anyhow::Result<()> {
        // With bridge_dir/src symlinked to member/src, the bridge's
        // [lib] path can be the relative `src/lib.rs` — cargo resolves
        // it through the symlink, and CARGO_MANIFEST_DIR semantics
        // stay consistent.
        let tmp = tempdir()?;
        let member_root = tmp.path().join("member");
        fs::create_dir_all(member_root.join("src"))?;
        fs::write(member_root.join("src/lib.rs"), "// lib\n")?;
        fs::write(member_root.join("Cargo.toml"), "[package]\nname = \"alpha\"\n")?;

        let mut m = member("alpha", Vec::new());
        m.manifest_dir = member_root.clone();
        m.src_lib_path = Some(member_root.join("src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], tmp.path().to_string_lossy().as_ref());
        let out_dir = tmp.path().join("bridges");
        fs::create_dir_all(&out_dir)?;
        write_bridge_crate(&plan, &m, &out_dir)?;

        let rendered =
            fs::read_to_string(out_dir.join(bridge_dir_name(&m)).join("Cargo.toml"))?;
        anyhow::ensure!(
            rendered.contains("path = \"src/lib.rs\""),
            "bridge [lib] path must be relative `src/lib.rs` (resolves through symlink):\n{rendered}"
        );
        anyhow::ensure!(
            !rendered.contains(&format!(
                "path = \"{}\"",
                member_root.join("src/lib.rs").to_string_lossy()
            )),
            "bridge [lib] path must NOT be the absolute member path anymore:\n{rendered}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn write_bridge_crate_wipes_stale_bridge_entries() -> anyhow::Result<()> {
        // If the member's tree changed between runs (entry renamed or
        // removed), stale symlinks in the bridge dir would misbehave.
        // write_bridge_crate must clear pre-existing entries before
        // repopulating.
        let tmp = tempdir()?;
        let member_root = tmp.path().join("member");
        fs::create_dir_all(member_root.join("src"))?;
        fs::write(member_root.join("src/lib.rs"), "// lib\n")?;
        fs::write(member_root.join("Cargo.toml"), "[package]\nname = \"alpha\"\n")?;

        let mut m = member("alpha", Vec::new());
        m.manifest_dir = member_root.clone();
        m.src_lib_path = Some(member_root.join("src/lib.rs"));
        m.has_src_rudzio_suite = true;

        let plan = plan_with_members(vec![m.clone()], tmp.path().to_string_lossy().as_ref());
        let out_dir = tmp.path().join("bridges");
        let bridge_root = out_dir.join(bridge_dir_name(&m));
        fs::create_dir_all(&bridge_root)?;
        // Pre-populate with a stale entry that should be cleaned up.
        fs::write(bridge_root.join("old_stale.txt"), "stale\n")?;

        write_bridge_crate(&plan, &m, &out_dir)?;

        anyhow::ensure!(
            !bridge_root.join("old_stale.txt").exists(),
            "stale file from previous run must be wiped before repopulating the bridge dir"
        );
        Ok(())
    }

    // ── E. check-cfg declarations ──────────────────────────────────────

    #[rudzio::test]
    fn bridge_build_rs_emits_check_cfg_for_rudzio_test() -> anyhow::Result<()> {
        // Silences `unexpected cfg` warnings for every `cfg(rudzio_test)`
        // reference in the member's source. Must appear alongside the
        // existing `cargo:rustc-cfg=rudzio_test` emit, in both binless
        // and bin-member variants.
        for bins in [Vec::new(), vec!["alpha-server".to_owned()]] {
            let mut m = member("alpha", Vec::new());
            m.manifest_dir = PathBuf::from("/abs/alpha");
            m.bin_names = bins.clone();
            let content = build_bridge_build_rs(&m);
            anyhow::ensure!(
                content.contains("cargo::rustc-check-cfg=cfg(rudzio_test)"),
                "bridge build.rs must emit check-cfg for rudzio_test (bins={bins:?}):\n{content}"
            );
        }
        Ok(())
    }

    #[rudzio::test]
    fn aggregator_build_rs_emits_check_cfg_for_rudzio_test() -> anyhow::Result<()> {
        // Same check-cfg declaration on the aggregator's own build.rs,
        // since the aggregator's lib + tests also reference
        // `cfg(rudzio_test)`.
        for bins in [Vec::new(), vec!["alpha-server".to_owned()]] {
            let mut m = member("alpha", vec![rudzio_dep()]);
            m.manifest_dir = PathBuf::from("/abs/alpha");
            m.src_lib_path = Some(PathBuf::from("/abs/alpha/src/lib.rs"));
            m.has_src_rudzio_suite = true;
            m.bin_names = bins.clone();

            let plan = plan_with_members(vec![m], "/abs");
            let out = tempdir()?;
            write_runner(&plan, out.path())?;
            let content = fs::read_to_string(out.path().join("build.rs"))?;
            anyhow::ensure!(
                content.contains("cargo::rustc-check-cfg=cfg(rudzio_test)"),
                "aggregator build.rs must emit check-cfg for rudzio_test (bins={bins:?}):\n{content}"
            );
        }
        Ok(())
    }

    // ── F. Reading rudzio feature opt-in from manifest ─────────────────

    #[rudzio::test]
    fn load_rudzio_activated_features_reads_array_from_metadata() -> anyhow::Result<()> {
        // `[package.metadata.rudzio] features = ["unit", "test"]` is the
        // opt-in vehicle for activating test-only features under bridge
        // compile. Mirrors the pattern of `[package.metadata.rudzio]
        // exclude = [...]` already used for test-file exclusion.
        let tmp = tempdir()?;
        let manifest = tmp.path().join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n\
             \n[package.metadata.rudzio]\nfeatures = [\"unit\", \"test\"]\n",
        )?;
        let got = load_rudzio_activated_features(&manifest)?;
        anyhow::ensure!(
            got == vec!["unit".to_owned(), "test".to_owned()],
            "expected [unit, test], got {got:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn load_rudzio_activated_features_missing_key_is_empty() -> anyhow::Result<()> {
        // A manifest without `[package.metadata.rudzio.features]` yields
        // an empty vec (no opt-in).
        let tmp = tempdir()?;
        let manifest = tmp.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n")?;
        let got = load_rudzio_activated_features(&manifest)?;
        anyhow::ensure!(got.is_empty(), "expected empty vec, got {got:?}");
        Ok(())
    }

    #[rudzio::test]
    fn load_rudzio_activated_features_non_array_is_error() -> anyhow::Result<()> {
        // Misconfiguration (string instead of array) is a hard error so
        // the user learns at aggregator-generation time, not silently.
        let tmp = tempdir()?;
        let manifest = tmp.path().join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n\
             \n[package.metadata.rudzio]\nfeatures = \"unit\"\n",
        )?;
        let err = load_rudzio_activated_features(&manifest).unwrap_err();
        anyhow::ensure!(
            err.to_string().contains("features") && err.to_string().contains("array"),
            "error must mention `features` and `array`, got: {err}"
        );
        Ok(())
    }
}
