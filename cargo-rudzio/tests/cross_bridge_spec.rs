//! Reproducers for the four cross-bridge generation gaps documented in
//! the workspace-wide `cargo rudzio test` blockers writeup. Each test
//! fails with the current `cargo_rudzio::generate` pipeline and passes
//! once the corresponding fix lands.
//!
//! Issues 1 and 4 are pure-shape assertions on `build_bridge_cargo_toml`
//! and run as fast unit tests against directly-constructed `Plan`s.
//! Issues 2 and 3 require driving the full pipeline through a synthetic
//! tempdir workspace because their symptoms (dev-only sibling cycle
//! elision; `force_bridge` opt-in for non-rudzio-test consumers) are
//! observable only end-to-end, after `read_dev_deps` and member-plan
//! construction. The end-to-end tests invoke the `cargo-rudzio` binary
//! as a subprocess via `env!("CARGO_BIN_EXE_cargo-rudzio")` so they
//! exercise the same code path a downstream user hits.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{
    DevDepSpec, MemberPlan, Plan, RudzioLocation, RudzioSpec, WorkspaceDepSpec,
    build_bridge_build_rs, build_bridge_cargo_toml,
};
use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::async_std::Runtime as AsyncStdRuntime;
use rudzio::runtime::compio::Runtime as CompioRuntime;
use rudzio::runtime::embassy::Runtime as EmbassyRuntime;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;
use rudzio::runtime::smol::Runtime as SmolRuntime;
use rudzio::runtime::tokio::{
    CurrentThread as TokioCurrentThread, Local as TokioLocal, Multithread as TokioMultithread,
};
use tempfile::TempDir;
use toml_edit::{DocumentMut, Item};

/// Construct a fully-bridged `MemberPlan` rooted at `<root>/<name>`.
///
/// The directory does not need to exist on disk; the pure-shape tests
/// feed this straight into `build_bridge_cargo_toml`, which never stats
/// its inputs.
fn bridged_member(name: &str, root: &Path) -> MemberPlan {
    let dir = root.join(name);
    let mut member = MemberPlan::new(name.to_owned(), dir.clone());
    member.has_src_rudzio_suite = true;
    // `has_lib()` is derived from `src_lib_path.is_some()`.
    member.src_lib_path = Some(dir.join("src").join("lib.rs"));
    "2024".clone_into(&mut member.edition);
    member
}

/// Build a `Plan` over `members` rooted at `root`, with `workspace_deps`
/// registered. The `target_directory` is set to `<root>/target`; rudzio
/// is registered as a registry-version dep (the bridge's own
/// `[dependencies]` injection only runs when no member already carries
/// rudzio, which is not exercised by these shape tests).
fn make_plan(
    members: Vec<MemberPlan>,
    root: &Path,
    workspace_deps: BTreeMap<String, WorkspaceDepSpec>,
) -> Plan {
    let rudzio_spec = RudzioSpec::new(
        RudzioLocation::Version {
            req: "0.1".to_owned(),
            registry: None,
        },
        Vec::new(),
        true,
    );
    let mut plan = Plan::new(
        Utf8PathBuf::from(root.to_string_lossy().into_owned()),
        Utf8PathBuf::from(format!("{}/target", root.display())),
        rudzio_spec,
    );
    plan.members = members;
    plan.workspace_deps = workspace_deps;
    plan
}

/// Synthetic workspace path used by the pure-shape tests. The directory
/// does not exist on disk.
fn synthetic_root() -> PathBuf {
    PathBuf::from("/nonexistent/cross-bridge-spec-ws")
}

/// Materialise a synthetic workspace at `root`: writes
/// `Cargo.toml` for the workspace, a stub `rudzio-stub/` package (named
/// `rudzio` so the cargo-rudzio "must depend on rudzio" filter accepts
/// the bridged members), then each entry in `members` as `<name>/Cargo.toml`
/// + `<name>/src/lib.rs`.
fn write_synthetic_workspace(root: &Path, members: &[(&str, &str, &str)]) -> anyhow::Result<()> {
    let mut workspace_toml = String::from("[workspace]\nmembers = [");
    let mut first = true;
    for (name, _, _) in members {
        if !first {
            workspace_toml.push_str(", ");
        }
        first = false;
        workspace_toml.push('"');
        workspace_toml.push_str(name);
        workspace_toml.push('"');
    }
    workspace_toml.push_str(", \"rudzio-stub\"]\nresolver = \"2\"\n\n");
    workspace_toml.push_str("[workspace.dependencies]\n");
    for (name, _, _) in members {
        workspace_toml.push_str(name);
        workspace_toml.push_str(" = { path = \"");
        workspace_toml.push_str(name);
        workspace_toml.push_str("\" }\n");
    }
    workspace_toml.push_str("rudzio = { path = \"rudzio-stub\" }\n");
    fs::write(root.join("Cargo.toml"), &workspace_toml)?;

    let stub_dir = root.join("rudzio-stub");
    fs::create_dir_all(stub_dir.join("src"))?;
    fs::write(
        stub_dir.join("Cargo.toml"),
        "[package]\n\
         name = \"rudzio\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [features]\n\
         common = []\n\
         \n\
         [lib]\n\
         path = \"src/lib.rs\"\n",
    )?;
    fs::write(stub_dir.join("src").join("lib.rs"), "")?;

    for (name, manifest, lib) in members {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("src"))?;
        fs::write(dir.join("Cargo.toml"), *manifest)?;
        fs::write(dir.join("src").join("lib.rs"), *lib)?;
    }
    Ok(())
}

/// Run `cargo-rudzio generate-runner --output <out>` from `cwd`,
/// surfacing captured stdout/stderr in the failure message when the
/// subprocess returns non-zero. The binary path comes from
/// `env!("CARGO_BIN_EXE_cargo-rudzio")`, which is set both by cargo's
/// own integration-test machinery and by the rudzio aggregator's
/// `expose_member_bins` build script.
///
/// `cargo_home` / `cargo_target_dir` are the per-test private cargo
/// state minted by [`IsolatedCargoSuite::context`] for this test.
/// Combined with `CARGO_NET_OFFLINE=true`, each fixture's spawned
/// cargo holds its own `.package-cache` flock and writes its
/// aggregator subtree to a tempdir that cleans itself at test drop.
fn generate_runner(
    cwd: &Path,
    out: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
) -> anyhow::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
        .current_dir(cwd)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .arg("generate-runner")
        .arg("--output")
        .arg(out)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "cargo-rudzio generate-runner failed (status {}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = TokioCurrentThread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = TokioLocal::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = CompioRuntime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = EmbassyRuntime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = FuturesThreadPool::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = AsyncStdRuntime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = SmolRuntime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        BTreeMap, DevDepSpec, DocumentMut, IsolatedCargoTest, Item, Path, PathBuf, TempDir,
        WorkspaceDepSpec, bridged_member, build_bridge_build_rs, build_bridge_cargo_toml,
        generate_runner, make_plan, synthetic_root, write_synthetic_workspace,
    };

    /// **Issue 1** — sibling-bridge dependency redirection.
    ///
    /// When bridge `A`'s rendered `[dependencies]` includes a name that is
    /// itself a bridged sibling, the entry must point at the SIBLING'S
    /// bridge — `path = "../<sibling>"`, `package = "<sibling>_rudzio_bridge"`.
    /// Today `render_dev_dep` copies the workspace dep's `path` verbatim,
    /// so the original member rlib gets linked alongside the bridge rlib
    /// and the aggregator drowns in `the trait bound X: Y is not satisfied`
    /// errors at link time.
    #[rudzio::test]
    fn sibling_bridge_redirection_in_bridge_dependencies() -> anyhow::Result<()> {
        let root = synthetic_root();
        let mut consumer = bridged_member("A", &root);
        let provider = bridged_member("B", &root);
        let mut consumer_dep_on_provider = DevDepSpec::new("B".to_owned());
        consumer_dep_on_provider.workspace_inherited = true;
        consumer.dev_deps = vec![consumer_dep_on_provider];

        let mut workspace_deps: BTreeMap<String, WorkspaceDepSpec> = BTreeMap::new();
        let mut provider_ws = WorkspaceDepSpec::new();
        provider_ws.path = Some(root.join("B"));
        let _previous: Option<WorkspaceDepSpec> =
            workspace_deps.insert("B".to_owned(), provider_ws);

        let plan = make_plan(vec![consumer.clone(), provider], &root, workspace_deps);
        let toml = build_bridge_cargo_toml(&plan, &consumer)?;
        let doc: DocumentMut = toml.parse()?;

        let entry = doc
            .get("dependencies")
            .and_then(|item| item.get("B"))
            .ok_or_else(|| anyhow::anyhow!("bridge A's [dependencies] missing key `B`"))?;
        let path = entry
            .get("path")
            .and_then(Item::as_str)
            .ok_or_else(|| anyhow::anyhow!("[dependencies].B.path missing"))?;
        let package = entry.get("package").and_then(Item::as_str);

        anyhow::ensure!(
            path == "../B",
            "bridge A → B should point at sibling bridge dir (path = \"../B\"), \
             not the original member path; got {path:?}"
        );
        anyhow::ensure!(
            package == Some("B_rudzio_bridge"),
            "bridge A → B should declare package = \"B_rudzio_bridge\" so cargo \
             unifies on a single B rlib; got {package:?}"
        );
        Ok(())
    }

    /// **Issue 4** — bridge `Cargo.toml` registers `cfg(rudzio_test)` via
    /// `[lints.rust]`.
    ///
    /// Each bridge build.rs already emits
    /// `cargo::rustc-check-cfg=cfg(rudzio_test)`, but downstream workspaces
    /// still see `unexpected cfg condition name: rudzio_test` warnings during
    /// the metadata / pre-build phase against `cfg_attr(any(test, rudzio_test), ...)`
    /// in member src. Belt-and-suspenders: every bridge `Cargo.toml` carries
    /// a `[lints.rust]` block that registers `rudzio_test` as a known cfg,
    /// regardless of which cargo/rustc version is consuming the build script.
    #[rudzio::test]
    fn bridge_cargo_toml_registers_rudzio_test_cfg() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member("A", &root);
        let plan = make_plan(vec![member.clone()], &root, BTreeMap::new());
        let toml = build_bridge_cargo_toml(&plan, &member)?;
        let doc: DocumentMut = toml.parse()?;

        let lints_rust = doc
            .get("lints")
            .and_then(|item| item.get("rust"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "bridge Cargo.toml missing [lints.rust]; nothing registers \
                     `cfg(rudzio_test)` as a known cfg name"
                )
            })?;
        let unexpected_cfgs = lints_rust.get("unexpected_cfgs").ok_or_else(|| {
            anyhow::anyhow!(
                "[lints.rust].unexpected_cfgs missing -- required to register \
                 `cfg(rudzio_test)` so member src using `cfg_attr(rudzio_test, ...)` \
                 doesn't draw `unexpected cfg condition name` warnings"
            )
        })?;
        let check_cfg = unexpected_cfgs
            .get("check-cfg")
            .and_then(Item::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!("[lints.rust].unexpected_cfgs.check-cfg missing or not an array")
            })?;
        let registered: Vec<&str> = check_cfg
            .iter()
            .filter_map(toml_edit::Value::as_str)
            .collect();
        anyhow::ensure!(
            registered.contains(&"cfg(rudzio_test)"),
            "[lints.rust].unexpected_cfgs.check-cfg must include `cfg(rudzio_test)`; \
             got {registered:?}"
        );
        Ok(())
    }

    /// **Issue 3** — `[package.metadata.rudzio] force_bridge = true` opt-in.
    ///
    /// `consumer` has a regular `[dependencies] shared = ...`, where `shared`
    /// is a bridged sibling. `consumer` itself carries no rudzio surface in
    /// `src/**`, so `bridge_applies_to(consumer)` returns false today and
    /// the aggregator gets `consumer → shared (original)` AND
    /// `shared (bridge)`, producing two distinct `shared` rlibs at link
    /// time. The fix: when `[package.metadata.rudzio] force_bridge = true`
    /// is set, bridge the consumer regardless of whether its own src
    /// references rudzio.
    #[rudzio::test]
    fn force_bridge_metadata_opts_a_consumer_into_bridging(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();

        let consumer_toml = "\
[package]
name = \"consumer\"
version = \"0.0.0\"
edition = \"2021\"
publish = false

[package.metadata.rudzio]
force_bridge = true

[lib]
path = \"src/lib.rs\"

[dependencies]
shared = { workspace = true }

[dev-dependencies]
rudzio = { workspace = true, features = [\"common\"] }
";
        // No `rudzio_test` substring anywhere — detect_src_rudzio_suite
        // returns false, so the only path to bridging this member is the
        // `force_bridge = true` opt-in.
        let consumer_lib_rs = "pub fn _ok() {}\n";

        let shared_toml = "\
[package]
name = \"shared\"
version = \"0.0.0\"
edition = \"2021\"
publish = false

[lib]
path = \"src/lib.rs\"

[dependencies]

[dev-dependencies]
rudzio = { workspace = true, features = [\"common\"] }
";
        let shared_lib_rs = "// rudzio_test marker\npub fn _ok() {}\n";

        write_synthetic_workspace(
            root,
            &[
                ("consumer", consumer_toml, consumer_lib_rs),
                ("shared", shared_toml, shared_lib_rs),
            ],
        )?;

        let out: PathBuf = root.join("agg");
        generate_runner(root, &out, ctx.cargo_home(), ctx.cargo_target_dir())?;

        let consumer_bridge = out.join("members").join("consumer").join("Cargo.toml");
        anyhow::ensure!(
            consumer_bridge.exists(),
            "consumer's bridge Cargo.toml must exist at {} when \
             `[package.metadata.rudzio] force_bridge = true` is set. Without \
             bridging, `consumer`'s path-dep on `shared` is linked alongside \
             `shared_rudzio_bridge`, producing two distinct `shared` rlibs and \
             trait-mismatch errors throughout the aggregator.",
            consumer_bridge.display()
        );
        Ok(())
    }

    /// **Sibling-dep feature union.**
    ///
    /// When workspace member A declares `B = { workspace = true,
    /// features = ["mock", "testing"] }`, those feature requests reach
    /// A's bridge's `[dependencies].B` entry (Issue 1 redirect carries
    /// them). Cargo SHOULD then activate them on B's bridge by feature
    /// unification. In practice that works for callers that resolve
    /// to the same bridge package — but the bridge's own `[features].default`
    /// is built from the member's own default + `[package.metadata.rudzio].features`
    /// only, so any feature only requested by sibling-bridge consumers is
    /// off when the bridge compiles standalone (e.g. when the aggregator's
    /// own `[dependencies]` references the bridge via path-only, no
    /// features). Members with optional cross-cuts like
    /// `testing = ["dep:rudzio"]` are forced to opt in via
    /// `[package.metadata.rudzio] features = [...]` to compensate.
    ///
    /// Fix: union every consumer's requested feature for the bridged
    /// member into the bridge's `[features].default`. Drops the
    /// `[package.metadata.rudzio] features` workaround for the common
    /// case where the union equals what the user wants enabled.
    #[rudzio::test]
    fn bridge_default_features_union_sibling_consumer_requests() -> anyhow::Result<()> {
        let root = synthetic_root();

        // Provider crate exposing `mock` and `testing` features. Neither
        // is in `default`, so without the union fix, the bridge's
        // default would also not include them.
        let mut provider = bridged_member("http", &root);
        provider.features = BTreeMap::from([
            ("mock".to_owned(), Vec::new()),
            ("testing".to_owned(), Vec::new()),
        ]);

        // Consumer that activates both features on the provider.
        let mut consumer = bridged_member("consumer", &root);
        let mut provider_dep = DevDepSpec::new("http".to_owned());
        provider_dep.workspace_inherited = true;
        provider_dep.features = vec!["mock".to_owned(), "testing".to_owned()];
        consumer.dev_deps = vec![provider_dep];

        let mut ws_deps: BTreeMap<String, WorkspaceDepSpec> = BTreeMap::new();
        let mut provider_ws = WorkspaceDepSpec::new();
        provider_ws.path = Some(root.join("http"));
        let _previous: Option<WorkspaceDepSpec> = ws_deps.insert("http".to_owned(), provider_ws);

        let plan = make_plan(vec![provider.clone(), consumer], &root, ws_deps);
        let provider_bridge_toml = build_bridge_cargo_toml(&plan, &provider)?;
        let doc: DocumentMut = provider_bridge_toml.parse()?;

        let default_feats = doc
            .get("features")
            .and_then(|item| item.get("default"))
            .and_then(Item::as_array)
            .ok_or_else(|| anyhow::anyhow!("provider bridge missing [features].default array"))?;
        let names: Vec<&str> = default_feats
            .iter()
            .filter_map(toml_edit::Value::as_str)
            .collect();

        anyhow::ensure!(
            names.contains(&"mock") && names.contains(&"testing"),
            "provider bridge's [features].default must union features that \
             sibling consumers request on it (so the bridge's standalone \
             compile activates them too). Got default = {names:?}; expected \
             to include `mock` and `testing`."
        );
        Ok(())
    }

    /// **Bridge proc-macro `CARGO_MANIFEST_DIR` gap.**
    ///
    /// Path-resolving proc-macros (`include_str!`, `include_bytes!`,
    /// `sqlx::migrate!`, `refinery::embed_migrations!`, askama
    /// templates, …) read the rustc-set `CARGO_MANIFEST_DIR` and look
    /// for paths relative to it. Under bridge compile, that env var
    /// points at the bridge dir (`target/.../members/<member>/`), not
    /// the member's real dir. Top-level entries are symlinked into the
    /// bridge so most direct references work, but `..`-based paths
    /// (e.g. `embed_migrations!("../shared/migrations")` from a
    /// `crates/storage/postgres/` member resolving to a sibling
    /// `crates/storage/migrations/`) and any code that reads
    /// `env!("CARGO_MANIFEST_DIR")` for absolute-path comparisons
    /// silently get the wrong answer.
    ///
    /// Fix: bridge `build.rs` emits
    /// `cargo:rustc-env=CARGO_MANIFEST_DIR=<original member dir>` so
    /// the bridge's compile unit sees the same manifest dir cargo would
    /// set under stock per-crate builds.
    #[rudzio::test]
    fn bridge_build_rs_overrides_cargo_manifest_dir_to_member_dir() -> anyhow::Result<()> {
        let root = synthetic_root();
        let alpha = bridged_member("alpha", &root);
        let alpha_dir = alpha.manifest_dir.to_string_lossy().into_owned();

        let build_rs = build_bridge_build_rs(&alpha);

        anyhow::ensure!(
            build_rs.contains(&format!("cargo:rustc-env=CARGO_MANIFEST_DIR={alpha_dir}")),
            "bridge build.rs must emit \
             `cargo:rustc-env=CARGO_MANIFEST_DIR=<member dir>` so path-resolving \
             proc-macros (include_str!, sqlx::migrate!, embed_migrations!, …) \
             find files relative to the original member dir instead of the bridge \
             dir. Got:\n{build_rs}"
        );
        Ok(())
    }
}
