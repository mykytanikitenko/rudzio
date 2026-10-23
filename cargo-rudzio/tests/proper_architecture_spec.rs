//! Architectural contract for the rudzio v2 two-layer pipeline.
//!
//! After the v2 refactor, bridges are complete per-member compile units
//! (own lib + tests + bins + env! parity overrides), and the aggregator
//! is a thin linker glue — one `extern crate <bridge>;` per member plus
//! `#[rudzio::main] fn main() {}`. The cascade of nested `cargo build`
//! invocations (the legacy `expose_member_bins` shim that produced the
//! `.package-cache` flock deadlock and the parallel-invocation race
//! signatures fixed in commit `00f5e6c`) is architecturally impossible
//! after this refactor: no generated build.rs spawns cargo.
//!
//! Each test pins one v2 invariant and fails against the current (pre-
//! refactor) code with a predictable error. Together they form the
//! contract the v2 implementation must satisfy.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use cargo_metadata::camino::Utf8PathBuf;
use cargo_rudzio::generate::{
    MemberBin, MemberPlan, Plan, RudzioLocation, RudzioSpec, bridge_applies_to, bridge_dir_name,
    build_bridge_build_rs, build_bridge_cargo_toml, build_build_rs, build_main_rs,
    scan_proc_macro_members_in_plan, write_runner,
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
use tempfile::TempDir;
use toml_edit::{DocumentMut, Item};

/// Construct a fully-bridged `MemberPlan` (lib + suite) without bins.
fn bridged_member(name: &str, root: &Path) -> MemberPlan {
    let dir = root.join(name);
    let mut member = MemberPlan::new(name.to_owned(), dir.clone());
    member.has_src_rudzio_suite = true;
    // `has_lib()` is derived from `src_lib_path.is_some()`.
    member.src_lib_path = Some(dir.join("src").join("lib.rs"));
    "2024".clone_into(&mut member.edition);
    member
}

/// Construct a bridged `MemberPlan` that also declares `[[bin]]` targets.
///
/// Each bin name is paired with a synthetic absolute source path under
/// `<root>/<name>/src/bin/<bin>.rs`; the path does not need to exist on
/// disk for pure-shape assertions.
fn bridged_member_with_bins(name: &str, root: &Path, bins: &[&str]) -> MemberPlan {
    let mut member = bridged_member(name, root);
    let dir = root.join(name);
    member.bin_targets = bins
        .iter()
        .map(|bin| {
            MemberBin::new(
                (*bin).to_owned(),
                dir.join("src").join("bin").join(format!("{bin}.rs")),
            )
        })
        .collect();
    member
}

/// Construct a bin-only `MemberPlan` (no lib, no suite).
///
/// Under v2 these must still produce a bridge so the bin compiles inside
/// the bridge crate (with member-name `cargo:rustc-env` overrides), but
/// the current `bridge_applies_to` predicate rejects them outright.
fn bin_only_member(name: &str, root: &Path, bins: &[&str]) -> MemberPlan {
    let dir = root.join(name);
    let mut member = MemberPlan::new(name.to_owned(), dir.clone());
    // `has_lib()` returns false when `src_lib_path` is None.
    member.has_src_rudzio_suite = false;
    member.src_lib_path = None;
    "2024".clone_into(&mut member.edition);
    member.bin_targets = bins
        .iter()
        .map(|bin| {
            MemberBin::new(
                (*bin).to_owned(),
                dir.join("src").join("bin").join(format!("{bin}.rs")),
            )
        })
        .collect();
    member
}

/// Wrap `members` in a `Plan` rooted at `root`.
fn make_plan(members: Vec<MemberPlan>, root: &Path) -> Plan {
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
    plan.workspace_deps = BTreeMap::new();
    plan
}

/// Synthetic workspace root used by the pure-shape tests.
fn synthetic_root() -> PathBuf {
    PathBuf::from("/nonexistent/proper-arch-spec-ws")
}

/// Collect every `[[bin]] path = "..."` value out of a TOML document.
fn bin_path_entries(doc: &DocumentMut) -> Vec<String> {
    let Some(item) = doc.get("bin") else {
        return Vec::new();
    };
    let Some(array) = item.as_array_of_tables() else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|table| {
            table
                .get("path")
                .and_then(Item::as_value)
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .collect()
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
    use super::{
        DocumentMut, MemberBin, TempDir, bin_only_member, bin_path_entries, bridge_applies_to,
        bridge_dir_name, bridged_member, bridged_member_with_bins, build_bridge_build_rs,
        build_bridge_cargo_toml, build_build_rs, build_main_rs, fs, make_plan,
        scan_proc_macro_members_in_plan, synthetic_root, write_runner,
    };

    /// **Invariant 1.** Bridge `build.rs` source must not spawn `cargo`.
    ///
    /// The legacy `expose_member_bins` shim was the source of the
    /// `.package-cache` flock cascade. Under v2 the bridge declares
    /// `[[bin]]` entries in its own `Cargo.toml` and the build script
    /// emits `cargo:rustc-env=CARGO_BIN_EXE_<n>=<path>` directives
    /// computed from documented cargo env vars — no nested process spawn.
    #[rudzio::test]
    fn bridge_build_rs_does_not_spawn_cargo() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member_with_bins("alpha", &root, &["alpha-server", "alpha-cli"]);
        let content = build_bridge_build_rs(&member)?;

        anyhow::ensure!(
            !content.contains("Command::new(\"cargo\")"),
            "bridge build.rs must not spawn cargo (cascade source):\n{content}"
        );
        anyhow::ensure!(
            !content.contains("expose_member_bins"),
            "bridge build.rs must not invoke the legacy expose_member_bins helper:\n{content}"
        );
        Ok(())
    }

    /// **Invariant 2.** Aggregator `build.rs` source must not spawn `cargo`.
    ///
    /// Aggregator's role shrinks to emitting `cargo:rustc-cfg=rudzio_test`
    /// + check-cfg. Per-member work moves into each bridge's build script.
    #[rudzio::test]
    fn aggregator_build_rs_does_not_spawn_cargo() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member_with_bins("alpha", &root, &["alpha-server"]);
        let plan = make_plan(vec![member], &root);
        let content = build_build_rs(&plan);

        anyhow::ensure!(
            !content.contains("Command::new(\"cargo\")"),
            "aggregator build.rs must not spawn cargo:\n{content}"
        );
        anyhow::ensure!(
            !content.contains("expose_member_bins"),
            "aggregator build.rs must not invoke the legacy expose_member_bins helper:\n{content}"
        );
        Ok(())
    }

    /// **Invariant 3.** Bridge `Cargo.toml` declares `[[bin]]` entries per member bin.
    ///
    /// The bridge OWNS the compile work for each member bin. Cargo
    /// compiles the bin from `<member>/src/bin/<n>.rs` as part of the
    /// bridge package; the aggregator never sees a member-bin compile.
    #[rudzio::test]
    fn bridge_cargo_toml_declares_member_bins_with_path() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member_with_bins("alpha", &root, &["alpha-server", "alpha-cli"]);
        let plan = make_plan(vec![member.clone()], &root);
        let rendered = build_bridge_cargo_toml(&plan, &member)?;
        let doc: DocumentMut = rendered.parse()?;

        let paths = bin_path_entries(&doc);
        anyhow::ensure!(
            paths.len() == 2,
            "bridge Cargo.toml must declare exactly 2 [[bin]] entries for a 2-bin member, got {paths:?}:\n{rendered}"
        );

        let bin0 = member
            .bin_targets
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing first bin target"))?;
        let bin1 = member
            .bin_targets
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("missing second bin target"))?;
        let expected_server = bin0.src_path.to_string_lossy().into_owned();
        let expected_cli = bin1.src_path.to_string_lossy().into_owned();
        anyhow::ensure!(
            paths.iter().any(|path| path == &expected_server),
            "bridge Cargo.toml must reference alpha-server source path {expected_server:?}, got {paths:?}"
        );
        anyhow::ensure!(
            paths.iter().any(|path| path == &expected_cli),
            "bridge Cargo.toml must reference alpha-cli source path {expected_cli:?}, got {paths:?}"
        );
        Ok(())
    }

    /// **Invariant 4.** Aggregator `Cargo.toml` declares no `[[bin]]` entries for member bins.
    ///
    /// Member bins live in their bridges. The aggregator's own runner
    /// bin (`rudzio-auto-runner`) is allowed — what's forbidden is any
    /// `[[bin]]` entry whose path points at a member's source.
    #[rudzio::test]
    fn aggregator_cargo_toml_does_not_declare_member_bins() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let member_dir = root.join("alpha");
        fs::create_dir_all(member_dir.join("src"))?;
        fs::write(member_dir.join("src").join("lib.rs"), "")?;

        let mut member = bridged_member_with_bins("alpha", &root, &["alpha-server"]);
        member.manifest_dir.clone_from(&member_dir);
        member.src_lib_path = Some(member_dir.join("src").join("lib.rs"));
        let first_bin = member
            .bin_targets
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing first bin target"))?;
        let member_bin_path = first_bin.src_path.to_string_lossy().into_owned();

        let plan = make_plan(vec![member], &root);
        let out_dir = root.join("aggregator");
        write_runner(&plan, &out_dir)?;
        let rendered = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        let doc: DocumentMut = rendered.parse()?;

        let paths = bin_path_entries(&doc);
        anyhow::ensure!(
            !paths.iter().any(|path| path == &member_bin_path),
            "aggregator Cargo.toml must not contain a [[bin]] path entry for member bin {member_bin_path:?} (member bins compile in their bridges); got {paths:?}:\n{rendered}"
        );
        Ok(())
    }

    /// **Invariant 5.** Bridge `build.rs` emits `cargo:rustc-env=CARGO_BIN_EXE_<n>=<path>` per bin.
    ///
    /// Replaces the legacy `expose_member_bins` cascade. Path arithmetic
    /// uses cargo's documented build-script env vars (`CARGO_TARGET_DIR`,
    /// PROFILE, optionally TARGET) so no nested cargo is required. The
    /// `rudzio::bin!()` macro retains a runtime `current_exe()` fallback
    /// as belt-and-suspenders against any path-shape drift.
    #[rudzio::test]
    fn bridge_build_rs_emits_cargo_bin_exe_per_bin() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member_with_bins("alpha", &root, &["alpha-server", "alpha-cli"]);
        let content = build_bridge_build_rs(&member)?;

        anyhow::ensure!(
            content.contains("CARGO_BIN_EXE_alpha-server")
                || content.contains("CARGO_BIN_EXE_alpha_server"),
            "bridge build.rs must emit cargo:rustc-env=CARGO_BIN_EXE_<n> for each bin (missing alpha-server):\n{content}"
        );
        anyhow::ensure!(
            content.contains("CARGO_BIN_EXE_alpha-cli")
                || content.contains("CARGO_BIN_EXE_alpha_cli"),
            "bridge build.rs must emit cargo:rustc-env=CARGO_BIN_EXE_<n> for each bin (missing alpha-cli):\n{content}"
        );
        Ok(())
    }

    /// **Invariant 6.** Bridge `build.rs` emits a `CARGO_PKG_NAME` override pointing at the member.
    ///
    /// Without this override, `env!("CARGO_PKG_NAME")` inside member src
    /// resolves to `<member>_rudzio_bridge` (the bridge crate's
    /// `[package].name`), breaking string-based identification (logging
    /// prefixes, panic messages, `clap` defaults). The override restores
    /// per-member env! parity.
    #[rudzio::test]
    fn bridge_build_rs_emits_cargo_pkg_name_override() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member("my-special-crate", &root);
        let content = build_bridge_build_rs(&member)?;

        anyhow::ensure!(
            content.contains("cargo:rustc-env=CARGO_PKG_NAME=my-special-crate"),
            "bridge build.rs must override CARGO_PKG_NAME so env!() inside member src returns the member's package name, not the bridge's:\n{content}"
        );
        Ok(())
    }

    /// **Invariant 7.** Aggregator `main.rs` does not `#[path]`-include member test sources.
    ///
    /// Tests live in the bridge under `cfg(rudzio_test)`; the aggregator
    /// links them in via `extern crate <bridge>;`. linkme's
    /// `distributed_slice` aggregates `TEST_TOKENS` across all linked
    /// compile units into the aggregator's final binary.
    #[rudzio::test]
    fn aggregator_main_rs_does_not_path_include_test_source() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member("alpha", &root);
        let plan = make_plan(vec![member], &root);
        let out = root.join("aggregator");
        let content = build_main_rs(&plan, &out);

        anyhow::ensure!(
            !content.contains("#[path"),
            "aggregator main.rs must not #[path]-include any source (tests live in bridges, linked via extern crate):\n{content}"
        );
        anyhow::ensure!(
            !content.contains("__rudzio_member_tests"),
            "aggregator main.rs must not declare the __rudzio_member_tests mod (proxies live in bridges now):\n{content}"
        );
        Ok(())
    }

    /// **Invariant 8.** Aggregator `main.rs` does not call `register_member_manifest_dirs`.
    ///
    /// The runtime registry exists today only because the aggregator's
    /// compile unit cannot emit per-member `cargo:rustc-env=CARGO_MANIFEST_DIR=...`
    /// (env vars are scoped per compile unit). Under v2 the bridge
    /// build.rs emits the override directly, so `rudzio::manifest_dir!()`
    /// becomes a thin `env!()` wrapper and the registry is deleted.
    #[rudzio::test]
    fn aggregator_main_rs_does_not_call_register_member_manifest_dirs() -> anyhow::Result<()> {
        let root = synthetic_root();
        let mut member = bridged_member("alpha", &root);
        // Test-bearing member triggers the legacy
        // `register_member_manifest_dirs` emission path today; under v2
        // that emission is removed regardless of test-file presence.
        member.test_files = vec![root.join("alpha").join("tests").join("integration.rs")];
        let plan = make_plan(vec![member], &root);
        let out = root.join("aggregator");
        let content = build_main_rs(&plan, &out);

        anyhow::ensure!(
            !content.contains("register_member_manifest_dirs"),
            "aggregator main.rs must not call register_member_manifest_dirs (runtime registry deleted under v2):\n{content}"
        );
        anyhow::ensure!(
            !content.contains("RUDZIO_MEMBER_MANIFEST_DIR_"),
            "aggregator main.rs must not read RUDZIO_MEMBER_MANIFEST_DIR_<member> env vars (bridge build.rs emits per-member CARGO_MANIFEST_DIR directly):\n{content}"
        );
        Ok(())
    }

    /// **Invariant 9.** Test proxies live under the bridge's `src/` directory.
    ///
    /// Today proxies are written under `<aggregator>/src/proxies/<binary_id>/`.
    /// Under v2 they move to `<bridge>/src/proxies/<binary_id>/` so the
    /// bridge's lib compilation includes them via `cfg(rudzio_test)`.
    #[rudzio::test]
    fn proxies_live_under_bridge_directory() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let member_dir = root.join("alpha");
        fs::create_dir_all(member_dir.join("src"))?;
        fs::create_dir_all(member_dir.join("tests"))?;
        fs::write(member_dir.join("src").join("lib.rs"), "")?;
        fs::write(
            member_dir.join("tests").join("integration.rs"),
            "#[rudzio::test]\nfn t() {}\n",
        )?;

        let mut member = bridged_member("alpha", &root);
        member.manifest_dir.clone_from(&member_dir);
        member.src_lib_path = Some(member_dir.join("src").join("lib.rs"));
        member.test_files = vec![member_dir.join("tests").join("integration.rs")];

        let plan = make_plan(vec![member.clone()], &root);
        let out_dir = root.join("aggregator");
        write_runner(&plan, &out_dir)?;

        let bridge_proxies = out_dir
            .join("members")
            .join(bridge_dir_name(&member))
            .join("src")
            .join("proxies");
        let aggregator_proxies = out_dir.join("src").join("proxies");
        anyhow::ensure!(
            bridge_proxies.exists(),
            "v2 places test proxies under <bridge>/src/proxies/, expected at {} (current code writes them under the aggregator instead)",
            bridge_proxies.display()
        );
        anyhow::ensure!(
            !aggregator_proxies.exists(),
            "aggregator must not contain a src/proxies/ tree (proxies moved to bridge), but it exists at {}",
            aggregator_proxies.display()
        );
        Ok(())
    }

    /// **Invariant 10.** `bridge_applies_to` accepts bin-only members.
    ///
    /// Under v2 bin compilation moves into the bridge, so every member
    /// that contributes at least one bin needs a bridge — even ones
    /// without a lib or rudzio surface. Today the predicate
    /// short-circuits on `has_lib`, leaving bin-only members to the
    /// aggregator's `expose_member_bins` cascade.
    #[rudzio::test]
    fn bridge_applies_to_includes_bin_only_members() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bin_only_member("only-bins", &root, &["the-bin"]);

        anyhow::ensure!(
            bridge_applies_to(&member),
            "bridge_applies_to must accept bin-only members under v2 (bin compilation moves into the bridge), but returned false"
        );
        Ok(())
    }

    /// **Invariant 11.** Each member bin path appears in exactly one `[[bin]] path` entry across bridge + aggregator manifests.
    ///
    /// Catches the latent double-compile defect where the legacy code
    /// invoked `expose_member_bins` from BOTH the bridge build.rs AND
    /// the aggregator build.rs for the same bin. Under v2 the bin's
    /// source path is declared exactly once — in the bridge's
    /// `Cargo.toml` — and cargo compiles it exactly once.
    #[rudzio::test]
    fn member_bin_declared_in_exactly_one_manifest() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let member_dir = root.join("alpha");
        fs::create_dir_all(member_dir.join("src"))?;
        fs::write(member_dir.join("src").join("lib.rs"), "")?;

        let mut member = bridged_member_with_bins("alpha", &root, &["alpha-bin"]);
        member.manifest_dir.clone_from(&member_dir);
        member.src_lib_path = Some(member_dir.join("src").join("lib.rs"));
        let first_bin = member
            .bin_targets
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing first bin target"))?;
        let expected_path = first_bin.src_path.to_string_lossy().into_owned();

        let plan = make_plan(vec![member.clone()], &root);

        let bridge_toml = build_bridge_cargo_toml(&plan, &member)?;
        let bridge_doc: DocumentMut = bridge_toml.parse()?;
        let bridge_paths = bin_path_entries(&bridge_doc);

        let out_dir = root.join("aggregator");
        write_runner(&plan, &out_dir)?;
        let agg_toml = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        let agg_doc: DocumentMut = agg_toml.parse()?;
        let agg_paths = bin_path_entries(&agg_doc);

        let bridge_hits = bridge_paths
            .iter()
            .filter(|path| **path == expected_path)
            .count();
        let agg_hits = agg_paths
            .iter()
            .filter(|path| **path == expected_path)
            .count();
        let total = bridge_hits.saturating_add(agg_hits);
        anyhow::ensure!(
            total == 1,
            "member bin source path {expected_path:?} must appear in exactly ONE [[bin]] path entry across bridge + aggregator manifests; got bridge={bridge_hits} aggregator={agg_hits} total={total}\nbridge:\n{bridge_toml}\naggregator:\n{agg_toml}"
        );
        anyhow::ensure!(
            bridge_hits == 1 && agg_hits == 0,
            "the single declaration must live in the bridge (not the aggregator); got bridge={bridge_hits} aggregator={agg_hits}"
        );
        Ok(())
    }

    /// **Invariant 12.** Bridge `build.rs` emits the full standard
    /// `CARGO_PKG_*` family (Decisions Log [I5]/[I6]/[I7]/[E10]).
    ///
    /// Beyond `CARGO_PKG_NAME` (Invariant 6), the bridge must mirror
    /// `CARGO_PKG_VERSION` + version components, descriptive metadata,
    /// `CARGO_CRATE_NAME`, and `CARGO_MANIFEST_LINKS` so member test
    /// code's `env!()` calls resolve against the member's manifest
    /// rather than the bridge's synthetic shell.
    #[rudzio::test]
    fn bridge_build_rs_emits_full_pkg_metadata_family() -> anyhow::Result<()> {
        let root = synthetic_root();
        let mut member = bridged_member("alpha-crate", &root);
        "1.2.3-beta.4".clone_into(&mut member.version);
        "1".clone_into(&mut member.version_major);
        "2".clone_into(&mut member.version_minor);
        "3".clone_into(&mut member.version_patch);
        "beta.4".clone_into(&mut member.version_pre);
        "Alice:Bob".clone_into(&mut member.authors);
        "A test crate".clone_into(&mut member.description);
        "MIT".clone_into(&mut member.license);
        "https://example.com".clone_into(&mut member.homepage);
        "https://github.com/example/alpha-crate".clone_into(&mut member.repository);
        "1.75".clone_into(&mut member.rust_version);
        "native-lib".clone_into(&mut member.links);
        let content = build_bridge_build_rs(&member)?;

        let required = [
            "cargo:rustc-env=CARGO_PKG_VERSION=1.2.3-beta.4",
            "cargo:rustc-env=CARGO_PKG_VERSION_MAJOR=1",
            "cargo:rustc-env=CARGO_PKG_VERSION_MINOR=2",
            "cargo:rustc-env=CARGO_PKG_VERSION_PATCH=3",
            "cargo:rustc-env=CARGO_PKG_VERSION_PRE=beta.4",
            "cargo:rustc-env=CARGO_PKG_AUTHORS=Alice:Bob",
            "cargo:rustc-env=CARGO_PKG_DESCRIPTION=A test crate",
            "cargo:rustc-env=CARGO_PKG_LICENSE=MIT",
            "cargo:rustc-env=CARGO_PKG_HOMEPAGE=https://example.com",
            "cargo:rustc-env=CARGO_PKG_REPOSITORY=https://github.com/example/alpha-crate",
            "cargo:rustc-env=CARGO_PKG_RUST_VERSION=1.75",
            "cargo:rustc-env=CARGO_CRATE_NAME=alpha_crate",
            "cargo:rustc-env=CARGO_MANIFEST_LINKS=native-lib",
        ];
        for needle in required {
            anyhow::ensure!(
                content.contains(needle),
                "bridge build.rs missing `{needle}`:\n{content}"
            );
        }
        Ok(())
    }

    /// **Invariant 13.** `CARGO_BIN_EXE_<name>` env-var preserves hyphens
    /// in the bin name (Decisions Log [C6]).
    ///
    /// Cargo's documented convention is to emit the bin name
    /// **exactly as-is** in the env-var suffix: a bin named `alpha-cli`
    /// produces `CARGO_BIN_EXE_alpha-cli`, NOT `CARGO_BIN_EXE_alpha_cli`.
    /// (Only `crate_name_to_ident` sanitizes hyphens for package/lib
    /// names; bin names are unmodified.)
    #[rudzio::test]
    fn bridge_build_rs_preserves_hyphens_in_cargo_bin_exe_name() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member_with_bins("alpha", &root, &["alpha-cli", "beta-server"]);
        let content = build_bridge_build_rs(&member)?;

        anyhow::ensure!(
            content.contains("CARGO_BIN_EXE_alpha-cli"),
            "bridge build.rs must emit `CARGO_BIN_EXE_alpha-cli` (hyphen preserved, per cargo \
             docs); got:\n{content}"
        );
        anyhow::ensure!(
            content.contains("CARGO_BIN_EXE_beta-server"),
            "bridge build.rs must emit `CARGO_BIN_EXE_beta-server` (hyphen preserved); \
             got:\n{content}"
        );
        anyhow::ensure!(
            !content.contains("CARGO_BIN_EXE_alpha_cli"),
            "bridge build.rs must NOT sanitize hyphens for `CARGO_BIN_EXE_<n>` (sanitization \
             applies only to package / lib names); got:\n{content}"
        );
        Ok(())
    }

    /// **Invariant 14.** Bridge `build.rs` emits `cargo:rerun-if-changed`
    /// pointing at the MEMBER's real manifest dir (Decisions Log [E16]).
    ///
    /// Without an explicit real-path rerun directive, cargo's default
    /// file-watching scans the bridge dir where symlinks-vs-target
    /// tracking is subtle. Explicit real-path targets make the bridge
    /// cache invalidate reliably when member source changes.
    #[rudzio::test]
    fn bridge_build_rs_rerun_if_changed_uses_real_member_paths() -> anyhow::Result<()> {
        let root = synthetic_root();
        let member = bridged_member("alpha", &root);
        let content = build_bridge_build_rs(&member)?;

        let member_dir_str = member.manifest_dir.to_string_lossy().into_owned();
        let expected = format!("cargo:rerun-if-changed={member_dir_str}");
        anyhow::ensure!(
            content.contains(&expected),
            "bridge build.rs must emit `{expected}` so cargo reruns the bridge build when \
             the member's REAL source tree changes:\n{content}"
        );
        Ok(())
    }

    /// **Invariant 15.** Bin-only members get a bridge with a stub
    /// `[lib]` AND are extern-crated by the aggregator
    /// (Decisions Log [C2]/[C3]).
    ///
    /// Bin-only bridges must declare `[lib] name = ...` (so the
    /// aggregator's `extern crate <bridge>;` resolves), with a
    /// generated stub `src/lib.rs`. Linkme cross-crate aggregation
    /// harmlessly registers nothing from the stub.
    #[rudzio::test]
    fn bin_only_member_bridge_has_stub_lib_and_extern_crate() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().to_path_buf();
        let member_dir = root.join("only-bins");
        fs::create_dir_all(member_dir.join("src").join("bin"))?;
        fs::write(
            member_dir.join("src").join("bin").join("the-bin.rs"),
            "fn main() {}\n",
        )?;

        let mut member = bin_only_member("only-bins", &root, &["the-bin"]);
        member.manifest_dir.clone_from(&member_dir);
        let first_bin = member
            .bin_targets
            .first_mut()
            .ok_or_else(|| anyhow::anyhow!("bin_only_member must produce at least one bin"))?;
        *first_bin = MemberBin::new(
            "the-bin".to_owned(),
            member_dir.join("src").join("bin").join("the-bin.rs"),
        );

        let plan = make_plan(vec![member.clone()], &root);
        let out_dir = root.join("aggregator");
        write_runner(&plan, &out_dir)?;

        // Bridge Cargo.toml must declare [lib].
        let bridge_dir = out_dir.join("members").join(bridge_dir_name(&member));
        let bridge_toml = fs::read_to_string(bridge_dir.join("Cargo.toml"))?;
        let bridge_doc: DocumentMut = bridge_toml.parse()?;
        anyhow::ensure!(
            bridge_doc.get("lib").is_some(),
            "bin-only bridge Cargo.toml must declare [lib] (Decisions Log [C2]); got:\n{bridge_toml}"
        );

        // Bridge src/lib.rs must exist (the generated stub).
        let bridge_lib = bridge_dir.join("src").join("lib.rs");
        anyhow::ensure!(
            bridge_lib.exists(),
            "bin-only bridge must have a generated src/lib.rs stub at {}",
            bridge_lib.display()
        );
        let stub_body = fs::read_to_string(&bridge_lib)?;
        anyhow::ensure!(
            stub_body.contains("Stub bridge lib for bin-only member"),
            "stub lib body should carry the v2 marker comment; got:\n{stub_body}"
        );

        // Aggregator main.rs must `extern crate only_bins;`.
        let agg_main = fs::read_to_string(out_dir.join("src").join("main.rs"))?;
        anyhow::ensure!(
            agg_main.contains("extern crate only_bins;"),
            "aggregator main.rs must extern-crate the bin-only bridge uniformly (Decisions Log \
             [C3]); got:\n{agg_main}"
        );
        Ok(())
    }

    /// **Invariant 16.** `force_bridge` with a bin-only member yields
    /// the same v2 structure: stub lib + [[bin]] entries (Decisions Log [E19]).
    ///
    /// Uniform v2 behavior — no per-trigger-reason branching. Whether
    /// the bridge was triggered automatically (lib + rudzio surface),
    /// by `force_bridge`, or because the member has bins, the bridge
    /// looks the same.
    #[rudzio::test]
    fn force_bridge_with_bin_only_member_has_lib_stub_and_bins() -> anyhow::Result<()> {
        let root = synthetic_root();
        let mut member = bin_only_member("forced", &root, &["main-tool"]);
        member.force_bridge = true;

        anyhow::ensure!(
            bridge_applies_to(&member),
            "force_bridge + bin-only must be bridge-eligible under v2"
        );

        let plan = make_plan(vec![member.clone()], &root);
        let bridge_toml = build_bridge_cargo_toml(&plan, &member)?;
        let bridge_doc: DocumentMut = bridge_toml.parse()?;
        anyhow::ensure!(
            bridge_doc.get("lib").is_some(),
            "force_bridge'd bin-only member must still declare [lib]:\n{bridge_toml}"
        );
        let paths = bin_path_entries(&bridge_doc);
        let first_bin_path = member
            .bin_targets
            .first()
            .map(|bin| bin.src_path.to_string_lossy())
            .ok_or_else(|| anyhow::anyhow!("force_bridge'd member must have at least one bin"))?;
        let only_path = paths
            .first()
            .ok_or_else(|| anyhow::anyhow!("bridge declared no [[bin]] paths"))?;
        anyhow::ensure!(
            paths.len() == 1 && *only_path == first_bin_path,
            "force_bridge'd bin-only member must declare its [[bin]] entries:\n{bridge_toml}"
        );
        Ok(())
    }

    /// **Invariant 17.** Proc-macro members are skipped (Decisions Log [E7])
    /// with a per-member warning surfaced through the plan diagnostic.
    ///
    /// Cargo restricts proc-macro crate deps to proc-macro/build-dep
    /// crates, which conflicts with the v2 sibling-bridge dep mirror.
    /// Out of scope for v2; document via warning.
    #[rudzio::test]
    fn proc_macro_member_is_not_bridged_with_warning() -> anyhow::Result<()> {
        let root = synthetic_root();
        let mut member = bridged_member("derive-macros", &root);
        member.is_proc_macro = true;
        let plan = make_plan(vec![member.clone()], &root);

        anyhow::ensure!(
            !bridge_applies_to(&member),
            "proc-macro members must not be bridged in v2"
        );
        let warnings = scan_proc_macro_members_in_plan(&plan);
        anyhow::ensure!(
            warnings
                .iter()
                .any(|w| w.contains("derive-macros") && w.contains("proc-macro")),
            "scan_proc_macro_members_in_plan must surface a per-member warning, got {warnings:?}"
        );
        Ok(())
    }
}
