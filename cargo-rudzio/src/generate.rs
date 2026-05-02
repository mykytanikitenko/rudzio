use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Result as FmtResult, Write as _};
use std::fs;
use std::io::Result as IoResult;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use cargo_metadata::camino::Utf8PathBuf;
use cargo_metadata::{Metadata, MetadataCommand, Package, TargetKind};
use toml_edit::{Array, DocumentMut, Formatted, InlineTable, Item, Table, Value, value};

/// Crate name of the aggregator package this module emits.
const AGGREGATOR_NAME: &str = "rudzio-auto-runner";
/// Verbatim helper code embedded into generated `build.rs` files for bin-bearing members.
///
/// The aggregator and bridge crates inline this string into their
/// emitted `build.rs` so they can shell out to `cargo build --bins`
/// against the real member's manifest and re-export the resulting
/// binary paths via `CARGO_BIN_EXE_<bin>` env vars.
const BUILD_RS_HELPERS: &str = r#"use std::path::PathBuf;
use std::process::Command;

fn env_var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("env var `{name}` not set by cargo"))
}

fn expose_member_bins(pkg: &str, manifest_dir: &str, bins: &[&str]) -> Result<(), String> {
    let out_dir = PathBuf::from(env_var("OUT_DIR")?);
    let profile = env_var("PROFILE")?;
    let cargo = std::env::var_os("CARGO").ok_or_else(|| "CARGO env var not set".to_owned())?;
    let target_dir = out_dir.join("rudzio-auto-bin-cache").join(pkg);

    let manifest = PathBuf::from(manifest_dir).join("Cargo.toml");
    let mut cmd = Command::new(&cargo);
    cmd.arg("build")
        .arg("--bins")
        .arg("--manifest-path")
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_dir);
    if profile == "release" {
        cmd.arg("--release");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("spawning `cargo build --bins` for `{pkg}`: {e}"))?;
    if !status.success() {
        return Err(format!("`cargo build --bins` for `{pkg}` exited with {status}"));
    }

    let subdir = if profile == "release" { "release" } else { "debug" };
    for bin in bins {
        let bin_path = target_dir.join(subdir).join(bin);
        if !bin_path.exists() {
            return Err(format!(
                "expected `{}` after `cargo build --bins -p {pkg}`, not found",
                bin_path.display()
            ));
        }
        println!("cargo:rustc-env=CARGO_BIN_EXE_{bin}={}", bin_path.display());
    }
    println!("cargo:rerun-if-changed={manifest_dir}");
    Ok(())
}
"#;
/// Top-level member entries that must NOT be symlinked into the bridge dir.
///
/// They either collide with bridge-synthesised files (`Cargo.toml`,
/// `build.rs`), are noise cargo regenerates anyway (`Cargo.lock`), or
/// would be actively harmful (`target/` creates parallel-build recursion,
/// `.git/` confuses tooling that walks up).
const BRIDGE_SKIPLIST: &[&str] = &["Cargo.toml", "Cargo.lock", "build.rs", "target", ".git"];
/// Crate name used for the `rudzio` dependency lookup in member manifests.
const RUDZIO_DEP: &str = "rudzio";

/// Source of a git-based cargo dependency reference.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GitRef {
    /// Branch name.
    Branch(String),
    /// Specific commit revision (sha or short-sha).
    Rev(String),
    /// Tag name.
    Tag(String),
}

/// Resolved location for the `rudzio` dependency that the aggregator should emit.
///
/// Mirrors the three mutually exclusive ways cargo lets a crate reference
/// another: a local path, a git URL, or a registry version requirement.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RudzioLocation {
    /// Git URL plus an optional branch/rev/tag pin.
    Git {
        /// Optional rev/branch/tag pin (`None` means "default branch").
        reference: Option<GitRef>,
        /// Repository URL.
        url: String,
    },
    /// Filesystem path to the crate root.
    Path(PathBuf),
    /// Registry version requirement.
    Version(String),
}

/// Description of a single `[dev-dependencies]` (or `[dependencies]`) entry
/// pulled from a workspace member's manifest.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DevDepSpec {
    /// Features explicitly requested by the member.
    pub features: Vec<String>,
    /// Git URL when this dep references a git repo.
    pub git: Option<String>,
    /// Optional rev/branch/tag pin associated with `git`.
    pub git_ref: Option<GitRef>,
    /// Crate name (post-rename if `package = ...` was used).
    pub name: String,
    /// `optional = true` in the member's `[dependencies]` entry.
    ///
    /// Mirrored into the bridge so `dep:X` references in the bridge's
    /// `[features]` table resolve (cargo requires the dep to be
    /// optional for `dep:` syntax to be valid).
    pub optional: bool,
    /// Filesystem path when this dep references a local crate.
    pub path: Option<PathBuf>,
    /// The local entry name under which this dep appears, when `package = ...` renames it.
    pub rename: Option<String>,
    /// Whether the member opted to keep cargo's `default-features = true`.
    pub uses_default_features: bool,
    /// Cargo-format version requirement string (`""` when the entry has no `version` key).
    pub version_req: String,
    /// Raw spec from the member's Cargo.toml.
    ///
    /// Used when it says `workspace = true` and we need to defer to the
    /// workspace entry.
    pub workspace_inherited: bool,
}

/// Generator-side description of one workspace member.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MemberPlan {
    /// Names of the member's `[[bin]]` targets.
    pub bin_names: Vec<String>,
    /// Dev-dep entries the aggregator must re-emit so the pulled-in test sources compile.
    pub dev_deps: Vec<DevDepSpec>,
    /// Rust edition declared in the member's `[package] edition`.
    ///
    /// The bridge crate uses this so its generated manifest matches the
    /// compilation semantics of the real source tree it re-points at.
    pub edition: String,
    /// The member's own `[features]` table, mirrored verbatim into the bridge's `[features]`.
    ///
    /// So `cfg(feature = "...")` gates in member source resolve against
    /// the same universe of feature names they would under the member's
    /// own `cargo test`.
    pub features: BTreeMap<String, Vec<String>>,
    /// `true` iff the member declares a `[lib]` target.
    ///
    /// (Or has the implicit `src/lib.rs`.) Bin-only crates can't be
    /// listed as regular `[dependencies]` entries — they go into
    /// `[workspace.members]` instead.
    pub has_lib: bool,
    /// `true` iff the member has at least one `src/**` file with a rudzio suite or `rudzio_test` gate.
    ///
    /// Drives bridge-crate generation: the bridge exists specifically to
    /// make `[dev-dependencies]` visible to the member's src tree under
    /// `--cfg rudzio_test`, so members whose src has no rudzio surface
    /// don't need one.
    pub has_src_rudzio_suite: bool,
    /// Absolute path to the member's manifest directory.
    pub manifest_dir: PathBuf,
    /// Cargo package name.
    pub package_name: String,
    /// Features listed in `[package.metadata.rudzio] features = [...]`.
    ///
    /// The member's explicit opt-in for features that should be active
    /// under `cargo rudzio test` but aren't in `default`. These get
    /// unioned with the member's `default` to form the bridge's own
    /// `default` feature list.
    pub rudzio_activated_features: Vec<String>,
    /// Absolute path to the member's `src/lib.rs` (when `has_lib`).
    ///
    /// The bridge crate's `[lib] path` points here so cargo compiles the
    /// real source tree instead of the bridge dir.
    pub src_lib_path: Option<PathBuf>,
    /// Absolute paths to the member's integration-test source files.
    ///
    /// Excludes its `tests/main.rs` shim.
    pub test_files: Vec<PathBuf>,
}

/// Everything the generator needs to emit the aggregator.
///
/// Extracted from `cargo metadata` plus the workspace root's Cargo.toml.
#[derive(Debug)]
#[non_exhaustive]
pub struct Plan {
    /// Members of the workspace that actually depend on rudzio AND
    /// contribute at least one integration-test source file.
    pub members: Vec<MemberPlan>,
    /// Resolved `rudzio` dependency to emit in the aggregator's `[dependencies]` table.
    ///
    /// Derived from how the workspace's own members declare rudzio
    /// (path / git / version), with features unioned across every
    /// member's declaration plus `common` + `build`.
    pub rudzio_spec: RudzioSpec,
    /// Cargo's resolved `target_directory` (host's build cache root).
    pub target_directory: Utf8PathBuf,
    /// Path → version overrides keyed by dep name.
    ///
    /// Pulled from the workspace root's `[workspace.dependencies]`
    /// table. Used when a member's dev-dep entry says `workspace = true`.
    pub workspace_deps: BTreeMap<String, WorkspaceDepSpec>,
    /// Absolute path to the workspace root.
    pub workspace_root: Utf8PathBuf,
}

/// Resolved spec for the `rudzio` dependency the aggregator emits.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RudzioSpec {
    /// Where the dep is hosted (path / git / registry version).
    pub features: Vec<String>,
    /// Resolved location.
    pub location: RudzioLocation,
    /// Whether to keep cargo's `default-features = true`.
    pub uses_default_features: bool,
}

/// Per-member resolved view of the rudzio dep used internally by [`collect_rudzio_spec`].
struct ResolvedRudzio {
    /// Cargo features unioned across all of the member's `rudzio` declarations.
    features: Vec<String>,
    /// Git URL when the resolved dep is git-hosted.
    git: Option<String>,
    /// Optional rev/branch/tag pin associated with `git`.
    git_ref: Option<GitRef>,
    /// Source member's package name (used in error messages).
    member: String,
    /// Filesystem path when the resolved dep is local.
    path: Option<PathBuf>,
    /// Whether the member kept cargo's `default-features = true`.
    uses_default_features: bool,
    /// Cargo version requirement when the resolved dep is registry-hosted.
    version_req: Option<String>,
}

/// `[workspace.dependencies]` entry pulled from the workspace root's manifest.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct WorkspaceDepSpec {
    /// Cargo features explicitly enabled at the workspace level.
    pub features: Vec<String>,
    /// Git URL when the workspace dep is git-hosted.
    pub git: Option<String>,
    /// Optional rev/branch/tag pin associated with `git`.
    pub git_ref: Option<GitRef>,
    /// Filesystem path when the workspace dep is local.
    pub path: Option<PathBuf>,
    /// Whether to keep cargo's `default-features = true`.
    pub uses_default_features: bool,
    /// Registry version requirement (when present).
    pub version_req: Option<String>,
}

impl DevDepSpec {
    /// Construct a default `DevDepSpec` keyed only by name.
    ///
    /// All other fields are zero-valued (`uses_default_features = true`)
    /// matching cargo's own dev-dep defaults. Use direct field
    /// assignment to populate the rest.
    #[inline]
    #[must_use]
    pub const fn new(name: String) -> Self {
        Self {
            features: Vec::new(),
            git: None,
            git_ref: None,
            name,
            optional: false,
            path: None,
            rename: None,
            uses_default_features: true,
            version_req: String::new(),
            workspace_inherited: false,
        }
    }
}

impl MemberPlan {
    /// Construct a `MemberPlan` with empty / default fields except for the package name and manifest dir.
    ///
    /// The remaining fields are populated via direct assignment by the
    /// generator (and by tests) after construction.
    #[inline]
    #[must_use]
    pub const fn new(package_name: String, manifest_dir: PathBuf) -> Self {
        Self {
            bin_names: Vec::new(),
            dev_deps: Vec::new(),
            edition: String::new(),
            features: BTreeMap::new(),
            has_lib: false,
            has_src_rudzio_suite: false,
            manifest_dir,
            package_name,
            rudzio_activated_features: Vec::new(),
            src_lib_path: None,
            test_files: Vec::new(),
        }
    }
}

impl Plan {
    /// Default output directory for the generated aggregator crate.
    #[inline]
    #[must_use]
    pub fn default_output_dir(&self) -> PathBuf {
        PathBuf::from(self.target_directory.as_std_path()).join(AGGREGATOR_NAME)
    }

    /// Construct an empty `Plan` keyed by workspace root, target directory, and rudzio spec.
    #[inline]
    #[must_use]
    pub const fn new(
        workspace_root: Utf8PathBuf,
        target_directory: Utf8PathBuf,
        rudzio_spec: RudzioSpec,
    ) -> Self {
        Self {
            members: Vec::new(),
            rudzio_spec,
            target_directory,
            workspace_deps: BTreeMap::new(),
            workspace_root,
        }
    }

    /// Restrict the plan to members at or under any path in `roots`.
    ///
    /// Drop every member whose manifest dir is not at-or-under any of
    /// `roots` (recursive: a member at `roots[i]/sub/sub` still matches).
    /// Returns `Err` if no member survives — saves the user from a silent
    /// "0 tests" run when their path typo'd.
    ///
    /// # Errors
    ///
    /// Returns an error when no remaining member matches any of `roots`.
    #[inline]
    pub fn restrict_to_paths(&mut self, roots: &[PathBuf]) -> Result<()> {
        let abs_roots: Vec<PathBuf> = roots.iter().map(|root| canonicalize_or_keep(root)).collect();
        self.members.retain(|member| {
            let abs = canonicalize_or_keep(&member.manifest_dir);
            member_under_any_root(&abs, &abs_roots)
        });
        if self.members.is_empty() {
            let shown = roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("no rudzio crates found at-or-under: {shown}");
        }
        Ok(())
    }
}

impl RudzioSpec {
    /// Construct a `RudzioSpec` from its three required fields.
    #[inline]
    #[must_use]
    pub const fn new(
        location: RudzioLocation,
        features: Vec<String>,
        uses_default_features: bool,
    ) -> Self {
        Self {
            features,
            location,
            uses_default_features,
        }
    }
}

impl WorkspaceDepSpec {
    /// Construct a default `WorkspaceDepSpec`.
    ///
    /// All fields default to empty; `uses_default_features` matches
    /// cargo's own true default.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            features: Vec::new(),
            git: None,
            git_ref: None,
            path: None,
            uses_default_features: true,
            version_req: None,
        }
    }
}

impl Default for WorkspaceDepSpec {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Pure helper: is `member_dir` at-or-under any path in `roots`?
///
/// Both sides are expected to be already-canonicalized absolute paths so
/// comparison is component-wise. Lifted out of
/// [`Plan::restrict_to_paths`] so it's testable without touching the
/// filesystem.
#[inline]
#[must_use]
pub fn member_under_any_root(member_dir: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| member_dir.starts_with(root))
}

/// Canonicalize `path`, falling back to the input when the OS call fails.
fn canonicalize_or_keep(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reconcile the workspace's `cargo metadata` output into a `Plan` for the generator.
fn build_plan(metadata: &Metadata) -> Result<Plan> {
    let workspace_root_std = metadata.workspace_root.as_std_path();
    let workspace_deps =
        read_workspace_deps(workspace_root_std).context("reading workspace root Cargo.toml")?;

    let mut members: Vec<MemberPlan> = Vec::new();
    let mut runtime_features: BTreeSet<String> = BTreeSet::new();

    let member_ids: BTreeSet<_> = metadata.workspace_members.iter().collect();
    for pkg in &metadata.packages {
        if !member_ids.contains(&pkg.id) {
            continue;
        }
        // Must depend on rudzio somewhere (normal or dev).
        let rudzio_deps: Vec<_> = pkg
            .dependencies
            .iter()
            .filter(|dep| dep.name == RUDZIO_DEP)
            .collect();
        if rudzio_deps.is_empty() {
            continue;
        }
        for rdep in &rudzio_deps {
            for feat in &rdep.features {
                if feat.starts_with("runtime-") {
                    let _newly_inserted: bool = runtime_features.insert(feat.clone());
                }
            }
        }

        if let Some(plan) = build_member_plan(pkg)? {
            members.push(plan);
        }
    }

    members.sort_by(|left, right| left.package_name.cmp(&right.package_name));

    let initial_spec = collect_rudzio_spec(&members, &workspace_deps, workspace_root_std)
        .context("deriving rudzio dependency spec for the aggregator")?;
    let rudzio_spec = inject_required_features(initial_spec, &runtime_features);

    Ok(Plan {
        workspace_root: metadata.workspace_root.clone(),
        target_directory: metadata.target_directory.clone(),
        members,
        rudzio_spec,
        workspace_deps,
    })
}

/// Construct a `MemberPlan` for one workspace package.
///
/// Reads the member's manifest for excluded test files, dev-deps, and
/// rudzio-activated features, walks its `cargo metadata` targets to pick
/// up bin / lib status, and bundles them all together. Returns
/// `Ok(None)` only if the manifest path has no parent directory (which
/// would indicate a malformed `cargo metadata` payload — defensive
/// rather than load-bearing).
fn build_member_plan(pkg: &Package) -> Result<Option<MemberPlan>> {
    let Some(manifest_parent) = pkg.manifest_path.parent() else {
        return Ok(None);
    };
    let manifest_dir = manifest_parent.to_path_buf();
    let manifest_dir_std = manifest_dir.as_std_path().to_path_buf();

    let exclude_list =
        load_rudzio_exclude_list(pkg.manifest_path.as_std_path()).with_context(|| {
            format!(
                "loading `[package.metadata.rudzio].exclude` from {}",
                pkg.manifest_path.as_std_path().display()
            )
        })?;

    let test_files = discover_test_files(pkg, &manifest_dir_std, &exclude_list)
        .with_context(|| format!("discovering test files for `{}`", pkg.name))?;

    let bin_names: Vec<String> = pkg
        .targets
        .iter()
        .filter(|target| target.kind.iter().any(|kind| matches!(kind, TargetKind::Bin)))
        .map(|target| target.name.clone())
        .collect();

    let has_lib = pkg
        .targets
        .iter()
        .any(|target| target.kind.iter().any(|kind| matches!(kind, TargetKind::Lib)));

    let src_lib_path = pkg
        .targets
        .iter()
        .find(|target| target.kind.iter().any(|kind| matches!(kind, TargetKind::Lib)))
        .map(|target| target.src_path.as_std_path().to_path_buf());

    let edition = pkg.edition.to_string();

    let dev_deps = read_dev_deps(pkg.manifest_path.as_std_path()).with_context(|| {
        format!(
            "reading dev-deps from {}",
            pkg.manifest_path.as_std_path().display()
        )
    })?;

    let has_src_rudzio_suite = has_lib
        && src_lib_path
            .as_deref()
            .and_then(|lib| lib.parent())
            .is_some_and(detect_src_rudzio_suite);

    let features: BTreeMap<String, Vec<String>> = pkg
        .features
        .iter()
        .map(|(name, deps)| (name.clone(), deps.clone()))
        .collect();

    let rudzio_activated_features =
        load_rudzio_activated_features(pkg.manifest_path.as_std_path()).with_context(|| {
            format!(
                "loading `[package.metadata.rudzio].features` from {}",
                pkg.manifest_path.as_std_path().display()
            )
        })?;

    Ok(Some(MemberPlan {
        package_name: pkg.name.to_string(),
        manifest_dir: manifest_dir_std,
        test_files,
        bin_names,
        has_lib,
        dev_deps,
        edition,
        src_lib_path,
        has_src_rudzio_suite,
        features,
        rudzio_activated_features,
    }))
}

/// Substring scan of `src_root` for rudzio suite/`rudzio_test` markers.
///
/// Scans every `*.rs` file under `src_root` for the markers that
/// indicate a rudzio suite or `--cfg rudzio_test`-gated module lives
/// there: `rudzio::suite` and `rudzio_test`. False positives (e.g. the
/// strings in a comment) are harmless — they produce a bridge crate for
/// a member that didn't strictly need one. False negatives would mean
/// `cargo rudzio test --via-bridge` silently drops that member's src
/// tests, which is why we keep the substring set deliberately broad.
/// The scan walks sub-directories (`src/foo/bar.rs` counts) but does not
/// descend symlinks and ignores non-UTF-8 files (unreadable → assume no
/// markers, which matches "false negatives are worse than false
/// positives" conservatively in the other direction — if the file is
/// unreadable the aggregator build will surface its own error).
#[inline]
#[must_use]
pub fn detect_src_rudzio_suite(src_root: &Path) -> bool {
    if !src_root.is_dir() {
        return false;
    }
    let mut stack: Vec<PathBuf> = vec![src_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_symlink() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if text.contains("rudzio::suite") || text.contains("rudzio_test") {
                return true;
            }
        }
    }
    false
}

/// Find `#[cfg(test)]`-gated modules in `member`'s src tree that are not broadened for rudzio.
///
/// Scans `member`'s `src/**/*.rs` for `#[cfg(test)]`-gated modules that
/// are NOT broadened to `any(test, rudzio_test)` and whose file does
/// NOT mention `rudzio::suite` or `rudzio_test` anywhere. Each hit
/// means a module that compiles under `cargo test` but silently
/// vanishes under `cargo rudzio test`. Produce one warning per site.
///
/// Substring-based (like `detect_src_rudzio_suite`) — avoids a full
/// syn parse. False positives (e.g. `#[cfg(test)]` on a non-module
/// item like a fn) are acceptable: the warning wording is advisory
/// ("might be invisible"), and users who know the block is
/// intentionally test-only can silence by including `rudzio_test`
/// anywhere in the file (comment suffices).
#[inline]
#[must_use]
pub fn scan_unbroadened_cfg_test_mods(member: &MemberPlan) -> Vec<String> {
    let Some(lib) = member.src_lib_path.as_deref() else {
        return Vec::new();
    };
    let Some(src_root) = lib.parent() else {
        return Vec::new();
    };
    if !src_root.is_dir() {
        return Vec::new();
    }

    let mut out: Vec<String> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![src_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_symlink() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    for path in &files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        if text.contains("rudzio::suite") || text.contains("rudzio_test") {
            // User has opted in to rudzio in this file (either via a
            // suite attribute or via an explicit `rudzio_test` gate);
            // suppress warnings.
            continue;
        }
        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("#[cfg(test)]") {
                continue;
            }
            // Must be immediately followed (modulo other attrs) by a
            // `mod ...` — otherwise this is #[cfg(test)] on a fn /
            // impl / use, which is not our concern. Cheap heuristic:
            // look ahead up to 6 non-attribute lines for a `mod`.
            let next_line = line_idx.saturating_add(1);
            let tail: Vec<&str> = text
                .lines()
                .skip(next_line)
                .take(6)
                .filter(|tail_line| !tail_line.trim_start().is_empty())
                .collect();
            let next_non_attr = tail
                .iter()
                .find(|tail_line| !tail_line.trim_start().starts_with("#["));
            let gates_a_mod = next_non_attr.is_some_and(|next| {
                next.trim_start().starts_with("mod ")
                    || next.trim_start().starts_with("pub mod ")
            });
            if !gates_a_mod {
                continue;
            }
            out.push(format!(
                "{file}:{line}: `#[cfg(test)]` on a module without \
                 rudzio_test gate — this module may be invisible under \
                 `cargo rudzio test`. If it carries rudzio tests, \
                 broaden to `#[cfg(any(test, rudzio_test))]`. \
                 Otherwise add a `// rudzio_test` comment to silence \
                 this warning.",
                file = path.display(),
                line = line_idx.saturating_add(1),
            ));
        }
    }
    out
}

/// Plan-level convenience: scan every member's src tree and return the
/// union of warnings in member order (matching `Plan.members` which
/// `build_plan` sorts by package name, so output is deterministic).
#[inline]
#[must_use]
pub fn scan_unbroadened_cfg_test_mods_in_plan(plan: &Plan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for member in &plan.members {
        out.extend(scan_unbroadened_cfg_test_mods(member));
    }
    out
}

/// Add the features the aggregator unconditionally needs (`common`,
/// `build`) plus any `runtime-*` feature requested by at least one
/// member's metadata-resolved rudzio dep — `runtime-*` features come
/// from `cargo metadata`'s dependency view (which already factors in
/// renames / cfg-target gating), so members may activate runtimes that
/// don't appear in the raw dev-dep features list we collected from the
/// member's Cargo.toml.
fn inject_required_features(
    mut spec: RudzioSpec,
    runtime_features: &BTreeSet<String>,
) -> RudzioSpec {
    let mut feats: BTreeSet<String> = spec.features.into_iter().collect();
    let _common_inserted: bool = feats.insert("common".to_owned());
    let _build_inserted: bool = feats.insert("build".to_owned());
    for feat in runtime_features {
        let _runtime_inserted: bool = feats.insert(feat.clone());
    }
    spec.features = feats.into_iter().collect();
    spec
}

/// Walk every rudzio-using member's raw Cargo.toml and reconcile their
/// `rudzio` declarations into a single `RudzioSpec`.
///
/// Rules:
/// - A member can declare `rudzio` under `[dependencies]`,
///   `[dev-dependencies]`, or the `[target.'cfg(...)'.*]` variants of
///   either. (`read_dev_deps` already collects all four sections — so
///   `MemberPlan::dev_deps` is the right input.)
/// - `workspace = true` resolves via `[workspace.dependencies] rudzio =
///   { ... }` in the workspace root's Cargo.toml.
/// - Across members, all declarations must reference rudzio in a way
///   that can be unified into one location: at most one of `path` /
///   `git` may appear. If both surface, we error with a clear message
///   so the user fixes the inconsistency rather than getting a confused
///   compile error in the generated aggregator.
/// - Path beats version-only when both surface (path is more specific).
///   Git beats version-only the same way.
/// - Features union across every declaration; `default-features`
///   ANDs (default-on stays on only if every declaration agreed).
/// - When NO member declares rudzio in a parseable way (defensive — we
///   already filter to rudzio-using members upstream, so this only
///   triggers for malformed manifests), fall back to a path dep on the
///   workspace root with `default-features = true`. This preserves the
///   in-rudzio-repo dogfood behaviour even if every member's declaration
///   somehow becomes unreadable.
///
/// # Errors
///
/// Returns an error when members reference rudzio inconsistently (mixed
/// path/git locations) or when no declaration carries a usable
/// path/git/version source.
#[inline]
pub fn collect_rudzio_spec(
    members: &[MemberPlan],
    workspace_deps: &BTreeMap<String, WorkspaceDepSpec>,
    workspace_root: &Path,
) -> Result<RudzioSpec> {
    let resolved: Vec<ResolvedRudzio> = resolve_rudzio_declarations(members, workspace_deps)?;

    if resolved.is_empty() {
        // Defensive fallback: something is off (we already filtered to
        // rudzio-using members upstream, so this should not happen),
        // but emit a usable spec so the aggregator at least has a
        // chance to compile inside rudzio's own repo.
        return Ok(RudzioSpec {
            location: RudzioLocation::Path(workspace_root.to_path_buf()),
            features: Vec::new(),
            uses_default_features: true,
        });
    }

    // Inconsistency check: at most one of `path` / `git` across all
    // members. Mixing them produces a spec the aggregator can't honour.
    let path_holder = resolved.iter().find(|entry| entry.path.is_some());
    let git_holder = resolved.iter().find(|entry| entry.git.is_some());
    if let (Some(path_entry), Some(git_entry)) = (path_holder, git_holder) {
        bail!(
            "rudzio is declared inconsistently across workspace members: `{}` uses path={}, `{}` uses git={} — aggregator can't unify these",
            path_entry.member,
            path_entry
                .path
                .as_ref()
                .map(|displayable| displayable.display().to_string())
                .unwrap_or_default(),
            git_entry.member,
            git_entry.git.clone().unwrap_or_default(),
        );
    }

    // Pick location. Path > git > version. (Path beats version because
    // it pins to a specific local checkout; git beats version because
    // it pins to a specific revision.)
    let location = if let Some(path_buf) = path_holder.and_then(|entry| entry.path.clone()) {
        // Canonicalize so `.` / `..` segments inherited from member
        // manifests like `path = "."` don't leak into the aggregator's
        // emitted spec (cosmetic — cargo accepts both, but matching the
        // workspace_root form keeps regenerated output stable).
        let normalized = fs::canonicalize(&path_buf).unwrap_or(path_buf);
        RudzioLocation::Path(normalized)
    } else if let Some(url) = git_holder.and_then(|entry| entry.git.clone()) {
        // For the git ref, take the first non-None ref encountered (git
        // declarations without `rev`/`branch`/`tag` mean "default branch",
        // which we encode as None — cargo treats absence the same way).
        let reference = resolved.iter().find_map(|entry| entry.git_ref.clone());
        RudzioLocation::Git { url, reference }
    } else {
        // No path or git: every declaration is version-only (or
        // workspace-inherited from a version-only entry). Take the
        // first non-None version_req.
        let version = resolved
            .iter()
            .find_map(|entry| entry.version_req.clone())
            .ok_or_else(|| {
                anyhow!(
                    "rudzio is declared by workspace members but none of the declarations carry `path`, `git`, or `version` \u{2014} aggregator can't reference rudzio"
                )
            })?;
        RudzioLocation::Version(version)
    };

    let mut features: BTreeSet<String> = BTreeSet::new();
    let mut uses_default_features = true;
    for resolved_entry in &resolved {
        for feat in &resolved_entry.features {
            let _feature_inserted: bool = features.insert(feat.clone());
        }
        uses_default_features &= resolved_entry.uses_default_features;
    }

    Ok(RudzioSpec {
        location,
        features: features.into_iter().collect(),
        uses_default_features,
    })
}

/// Walk every member's dev-deps and build the resolved rudzio entry list used by `collect_rudzio_spec`.
fn resolve_rudzio_declarations(
    members: &[MemberPlan],
    workspace_deps: &BTreeMap<String, WorkspaceDepSpec>,
) -> Result<Vec<ResolvedRudzio>> {
    let mut resolved: Vec<ResolvedRudzio> = Vec::new();
    for member in members {
        for dep in &member.dev_deps {
            // `dep.name` is the package name post-rename. Rudzio is only
            // ever referenced by its own crate name (no rename), so a
            // straightforward name match is correct.
            if dep.name != RUDZIO_DEP {
                continue;
            }
            if dep.workspace_inherited {
                let ws = workspace_deps.get(RUDZIO_DEP).ok_or_else(|| {
                    anyhow!(
                        "member `{}` declares `rudzio = {{ workspace = true }}` but the workspace root has no `[workspace.dependencies.rudzio]` entry",
                        member.package_name
                    )
                })?;
                let mut feats: Vec<String> = ws.features.clone();
                feats.extend(dep.features.iter().cloned());
                resolved.push(ResolvedRudzio {
                    member: member.package_name.clone(),
                    path: ws.path.clone(),
                    git: ws.git.clone(),
                    git_ref: ws.git_ref.clone(),
                    version_req: ws.version_req.clone(),
                    features: feats,
                    uses_default_features: dep.uses_default_features
                        && ws.uses_default_features,
                });
            } else {
                let version_req = if dep.version_req.is_empty() {
                    None
                } else {
                    Some(dep.version_req.clone())
                };
                resolved.push(ResolvedRudzio {
                    member: member.package_name.clone(),
                    path: dep.path.clone(),
                    git: dep.git.clone(),
                    git_ref: dep.git_ref.clone(),
                    version_req,
                    features: dep.features.clone(),
                    uses_default_features: dep.uses_default_features,
                });
            }
        }
    }
    Ok(resolved)
}

/// Pick the right inline-table shape for the aggregator's rudzio dep:
/// route through rudzio's bridge when rudzio is itself a bridged
/// workspace member, otherwise fall back to the workspace-derived spec.
#[inline]
#[must_use]
fn aggregator_rudzio_inline_table(plan: &Plan) -> InlineTable {
    plan.members
        .iter()
        .find(|member| member.package_name == RUDZIO_DEP && bridge_applies_to(member))
        .map_or_else(
            || build_rudzio_inline_table(&plan.rudzio_spec),
            |member| build_rudzio_bridge_inline_table(member, &plan.rudzio_spec),
        )
}

/// Render the aggregator's rudzio dep as a sibling-bridge entry pointing
/// at `rudzio_member`'s generated bridge crate.
///
/// Used when rudzio is itself a workspace member of the consumer's
/// workspace (the rudzio repo dogfooding itself) — without this, the
/// aggregator's `[dependencies] rudzio = { path = ... }` would compile a
/// second rudzio rlib alongside the bridges' shared one, and at startup
/// the linkme `#[distributed_slice]` machinery rudzio uses for test
/// discovery panics with "duplicate `#[distributed_slice]`". Features
/// and `default-features` carry over from `spec` so the union of every
/// member's requested rudzio runtimes still reaches the bridge.
#[inline]
#[must_use]
pub fn build_rudzio_bridge_inline_table(
    rudzio_member: &MemberPlan,
    spec: &RudzioSpec,
) -> InlineTable {
    let mut tbl = InlineTable::new();
    tbl.insert(
        "path",
        Value::String(Formatted::new(format!(
            "./members/{}",
            bridge_dir_name(rudzio_member)
        ))),
    );
    tbl.insert(
        "package",
        Value::String(Formatted::new(bridge_package_name(rudzio_member))),
    );
    if !spec.features.is_empty() {
        let mut feats = Array::new();
        for feat in &spec.features {
            feats.push(Value::String(Formatted::new(feat.clone())));
        }
        tbl.insert("features", Value::Array(feats));
    }
    if !spec.uses_default_features {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }
    tbl
}

/// Render a `RudzioSpec` as a Cargo dependency inline table.
///
/// Emits the minimal correct shape: location keys (`path` / `git` + ref
/// / `version`) followed by `features` (only when non-empty) and
/// `default-features = false` (only when the user opted out).
#[inline]
#[must_use]
pub fn build_rudzio_inline_table(spec: &RudzioSpec) -> InlineTable {
    let mut tbl = InlineTable::new();
    match &spec.location {
        RudzioLocation::Path(path_buf) => {
            tbl.insert(
                "path",
                Value::String(Formatted::new(path_buf.to_string_lossy().into_owned())),
            );
        }
        RudzioLocation::Git { url, reference } => {
            tbl.insert("git", Value::String(Formatted::new(url.clone())));
            match reference {
                Some(GitRef::Rev(rev)) => {
                    tbl.insert("rev", Value::String(Formatted::new(rev.clone())));
                }
                Some(GitRef::Branch(branch)) => {
                    tbl.insert("branch", Value::String(Formatted::new(branch.clone())));
                }
                Some(GitRef::Tag(tag)) => {
                    tbl.insert("tag", Value::String(Formatted::new(tag.clone())));
                }
                None => {}
            }
        }
        RudzioLocation::Version(version) => {
            tbl.insert("version", Value::String(Formatted::new(version.clone())));
        }
    }
    if !spec.features.is_empty() {
        let mut feats = Array::new();
        for feat in &spec.features {
            feats.push(Value::String(Formatted::new(feat.clone())));
        }
        tbl.insert("features", Value::Array(feats));
    }
    if !spec.uses_default_features {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }
    tbl
}

/// For each member, list the integration-test source files that should
/// be `#[path]`-included in the aggregator.
///
/// Algorithm:
/// - If the manifest declares one or more explicit `[[test]]` entries,
///   use the `path` fields of each, minus any that resolve to the
///   `tests/main.rs` shim (which contains its own `#[rudzio::main]`).
/// - Otherwise (no explicit entries, or `autotests = false` with no
///   entries) fall back to scanning immediate children of `tests/*.rs`.
///
/// Files whose absolute path matches any entry in `exclude` (sourced from
/// `[package.metadata.rudzio].exclude` in the member's Cargo.toml) are
/// filtered out — useful for trybuild-driven tests whose manifest-dir-
/// relative paths would break when `#[path]`-included into the aggregator.
fn discover_test_files(
    pkg: &Package,
    manifest_dir: &Path,
    exclude: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let tests_dir = manifest_dir.join("tests");
    let main_shim = tests_dir.join("main.rs");

    // Explicit `[[test]]` entries, minus the aggregator shim (`tests/main.rs`
    // — the file containing its own `#[rudzio::main]`). Rudzio crates set
    // `autotests = false` and declare a single `[[test]] path = "tests/main.rs"`,
    // so the explicit list collapses to empty here and we fall through to
    // directory scanning.
    let explicit: Vec<PathBuf> = pkg
        .targets
        .iter()
        .filter(|target| target.kind.iter().any(|kind| matches!(kind, TargetKind::Test)))
        .map(|target| target.src_path.as_std_path().to_path_buf())
        .filter(|path_buf| path_buf != &main_shim)
        .collect();

    let mut files: Vec<PathBuf> = if explicit.is_empty() {
        if !tests_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for raw_entry in
            fs::read_dir(&tests_dir).with_context(|| format!("reading {}", tests_dir.display()))?
        {
            let entry = raw_entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            if path == main_shim {
                continue;
            }
            out.push(path);
        }
        out
    } else {
        explicit
    };

    if !exclude.is_empty() {
        files.retain(|file| !exclude.iter().any(|excluded| paths_equal(file, excluded)));
    }

    files.sort();
    files.dedup();
    Ok(files)
}

/// Load `[package.metadata.rudzio].exclude` from the member's Cargo.toml.
///
/// Returns absolute paths (each entry joined against the manifest's parent
/// directory). An absent section yields an empty vec; a present but non-
/// array-of-strings value is a hard error so misconfigurations are caught
/// at aggregator-generation time rather than silently ignored.
fn load_rudzio_exclude_list(manifest_path: &Path) -> Result<Vec<PathBuf>> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent"))?;

    let Some(package) = doc.get("package").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(metadata) = package.get("metadata").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(rudzio_meta) = metadata.get("rudzio").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(exclude_item) = rudzio_meta.get("exclude") else {
        return Ok(Vec::new());
    };
    let array = exclude_item.as_array().ok_or_else(|| {
        anyhow!(
            "`[package.metadata.rudzio].exclude` in {} must be an array of strings",
            manifest_path.display()
        )
    })?;

    let mut out: Vec<PathBuf> = Vec::with_capacity(array.len());
    for entry in array {
        let path_str = entry.as_str().ok_or_else(|| {
            anyhow!(
                "`[package.metadata.rudzio].exclude` in {} must contain only strings",
                manifest_path.display()
            )
        })?;
        out.push(manifest_dir.join(path_str));
    }
    Ok(out)
}

/// Read the `[package.metadata.rudzio] features = [...]` array from a member manifest.
///
/// These names are features the member opts in to under `cargo rudzio
/// test` — they become part of the bridge's own `default` feature list
/// so cargo activates them when compiling the bridge. Missing → empty
/// vec (no opt-in). Non-array value → hard error so misconfigurations
/// surface at aggregator-generation time.
///
/// # Errors
///
/// Returns an error if the manifest cannot be read/parsed, or if the
/// `[package.metadata.rudzio].features` value is present but not an
/// array of strings.
#[inline]
pub fn load_rudzio_activated_features(manifest_path: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let Some(package) = doc.get("package").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(metadata) = package.get("metadata").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(rudzio_meta) = metadata.get("rudzio").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(features_item) = rudzio_meta.get("features") else {
        return Ok(Vec::new());
    };
    let array = features_item.as_array().ok_or_else(|| {
        anyhow!(
            "`[package.metadata.rudzio].features` in {} must be an array of strings",
            manifest_path.display()
        )
    })?;

    let mut out: Vec<String> = Vec::with_capacity(array.len());
    for entry in array {
        let feature_str = entry.as_str().ok_or_else(|| {
            anyhow!(
                "`[package.metadata.rudzio].features` in {} must contain only strings",
                manifest_path.display()
            )
        })?;
        out.push(feature_str.to_owned());
    }
    Ok(out)
}

/// Parse the workspace root's `[workspace.dependencies]` table into a map of `WorkspaceDepSpec`.
fn read_workspace_deps(workspace_root: &Path) -> Result<BTreeMap<String, WorkspaceDepSpec>> {
    let manifest_path = workspace_root.join("Cargo.toml");
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let mut out: BTreeMap<String, WorkspaceDepSpec> = BTreeMap::new();
    let Some(ws) = doc.get("workspace").and_then(Item::as_table) else {
        return Ok(out);
    };
    let Some(deps) = ws.get("dependencies").and_then(Item::as_table) else {
        return Ok(out);
    };
    for (name, item) in deps {
        let spec = extract_workspace_dep_spec(item, workspace_root);
        drop(out.insert(name.to_owned(), spec));
    }
    Ok(out)
}

/// Extract a `WorkspaceDepSpec` from a single `[workspace.dependencies]` entry value.
fn extract_workspace_dep_spec(item: &Item, workspace_root: &Path) -> WorkspaceDepSpec {
    let mut spec = WorkspaceDepSpec {
        version_req: None,
        path: None,
        git: None,
        git_ref: None,
        features: Vec::new(),
        uses_default_features: true,
    };
    match item {
        Item::Value(Value::String(version_str)) => {
            spec.version_req = Some(version_str.value().to_owned());
        }
        Item::Value(Value::InlineTable(inline)) => {
            for (key, val) in inline {
                apply_ws_dep_field(&mut spec, key, val, workspace_root);
            }
        }
        Item::Table(table) => {
            for (key, item_val) in table {
                if let Some(val) = item_val.as_value() {
                    apply_ws_dep_field(&mut spec, key, val, workspace_root);
                }
            }
        }
        Item::None | Item::Value(_) | Item::ArrayOfTables(_) => {}
    }
    spec
}

/// Apply one `key = val` pair from a `[workspace.dependencies]` entry into `spec`.
fn apply_ws_dep_field(spec: &mut WorkspaceDepSpec, key: &str, val: &Value, base: &Path) {
    match key {
        "version" => {
            if let Some(text) = val.as_str() {
                spec.version_req = Some(text.to_owned());
            }
        }
        "path" => {
            if let Some(text) = val.as_str() {
                spec.path = Some(base.join(text));
            }
        }
        "git" => {
            if let Some(text) = val.as_str() {
                spec.git = Some(text.to_owned());
            }
        }
        "rev" => {
            if let Some(text) = val.as_str() {
                spec.git_ref = Some(GitRef::Rev(text.to_owned()));
            }
        }
        "branch" => {
            if let Some(text) = val.as_str() {
                spec.git_ref = Some(GitRef::Branch(text.to_owned()));
            }
        }
        "tag" => {
            if let Some(text) = val.as_str() {
                spec.git_ref = Some(GitRef::Tag(text.to_owned()));
            }
        }
        "features" => {
            if let Some(arr) = val.as_array() {
                spec.features = arr
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect();
            }
        }
        "default-features" => {
            if let Some(flag) = val.as_bool() {
                spec.uses_default_features = flag;
            }
        }
        _ => {}
    }
}

/// Parse both `[dependencies]` and `[dev-dependencies]` (plus their
/// `[target.'cfg(...)'.*]` variants) out of the member's Cargo.toml
/// verbatim. Normal deps are included because `#[path]`-inclusion of a
/// member's integration-test source pulls in references to the member's
/// regular deps (e.g. `macro-internals/tests/transform.rs` says
/// `use syn::...`, which only resolves if `syn` is a dep of the crate
/// actually compiling the source — here, the aggregator). Dev-deps are
/// included for the same reason (`trybuild`, `libc` in fixtures). The
/// `workspace = true` flag is preserved so the aggregator defers to the
/// workspace root's pinned version.
fn read_dev_deps(manifest_path: &Path) -> Result<Vec<DevDepSpec>> {
    let sections = ["dependencies", "dev-dependencies"];
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent"))?;

    let mut out: Vec<DevDepSpec> = Vec::new();
    for section in sections {
        collect_dev_deps(&doc, &[section], manifest_dir, &mut out);
    }

    if let Some(target_tbl) = doc.get("target").and_then(Item::as_table) {
        for (_cfg, cfg_item) in target_tbl {
            let Some(cfg_tbl) = cfg_item.as_table() else {
                continue;
            };
            for section in sections {
                if let Some(Item::Table(deps_tbl)) = cfg_tbl.get(section) {
                    for (name, item) in deps_tbl {
                        if let Some(spec) = parse_dev_dep_entry(name, item, manifest_dir) {
                            out.push(spec);
                        }
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Walk `doc` to the table at `path` and append every entry as a `DevDepSpec` into `out`.
fn collect_dev_deps(
    doc: &DocumentMut,
    path: &[&str],
    manifest_dir: &Path,
    out: &mut Vec<DevDepSpec>,
) {
    let mut cur: &Item = doc.as_item();
    for key in path {
        let Some(tbl) = cur.as_table() else { return };
        let Some(next) = tbl.get(key) else { return };
        cur = next;
    }
    let Some(deps_tbl) = cur.as_table() else {
        return;
    };
    for (name, item) in deps_tbl {
        if let Some(spec) = parse_dev_dep_entry(name, item, manifest_dir) {
            out.push(spec);
        }
    }
}

/// Parse a single dev/normal dep entry into a `DevDepSpec`, returning `None` for unsupported shapes.
fn parse_dev_dep_entry(name: &str, item: &Item, manifest_dir: &Path) -> Option<DevDepSpec> {
    let mut spec = DevDepSpec {
        name: name.to_owned(),
        rename: None,
        version_req: String::new(),
        path: None,
        git: None,
        git_ref: None,
        features: Vec::new(),
        uses_default_features: true,
        workspace_inherited: false,
        optional: false,
    };
    match item {
        Item::Value(Value::String(version_str)) => {
            version_str.value().clone_into(&mut spec.version_req);
            Some(spec)
        }
        Item::Value(Value::InlineTable(inline)) => {
            fill_dev_dep_from_pairs(&mut spec, inline.iter(), manifest_dir);
            Some(spec)
        }
        Item::Table(table) => {
            fill_dev_dep_from_table(&mut spec, table, manifest_dir);
            Some(spec)
        }
        Item::None | Item::Value(_) | Item::ArrayOfTables(_) => None,
    }
}

/// Apply every `(key, value)` pair from an iterator into `spec`.
fn fill_dev_dep_from_pairs<'pair>(
    spec: &mut DevDepSpec,
    entries: impl Iterator<Item = (&'pair str, &'pair Value)>,
    manifest_dir: &Path,
) {
    for (key, val) in entries {
        apply_dev_dep_field(spec, key, val, manifest_dir);
    }
}

/// Apply every value-typed cell of a TOML `Table` into `spec`.
fn fill_dev_dep_from_table(spec: &mut DevDepSpec, table: &Table, manifest_dir: &Path) {
    for (key, item) in table {
        if let Some(val) = item.as_value() {
            apply_dev_dep_field(spec, key, val, manifest_dir);
        }
    }
}

/// Apply one `key = val` pair from a dev/normal dep entry into `spec`.
fn apply_dev_dep_field(spec: &mut DevDepSpec, key: &str, val: &Value, manifest_dir: &Path) {
    match key {
        "workspace"
            if val.as_bool() == Some(true) => {
                spec.workspace_inherited = true;
            }
        "version" => {
            if let Some(text) = val.as_str() {
                text.clone_into(&mut spec.version_req);
            }
        }
        "path" => {
            if let Some(text) = val.as_str() {
                spec.path = Some(manifest_dir.join(text));
            }
        }
        "git" => {
            if let Some(text) = val.as_str() {
                spec.git = Some(text.to_owned());
            }
        }
        "rev" => {
            if let Some(text) = val.as_str() {
                spec.git_ref = Some(GitRef::Rev(text.to_owned()));
            }
        }
        "branch" => {
            if let Some(text) = val.as_str() {
                spec.git_ref = Some(GitRef::Branch(text.to_owned()));
            }
        }
        "tag" => {
            if let Some(text) = val.as_str() {
                spec.git_ref = Some(GitRef::Tag(text.to_owned()));
            }
        }
        "features" => {
            if let Some(arr) = val.as_array() {
                spec.features = arr
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect();
            }
        }
        "default-features" => {
            if let Some(flag) = val.as_bool() {
                spec.uses_default_features = flag;
            }
        }
        "optional" => {
            if let Some(flag) = val.as_bool() {
                spec.optional = flag;
            }
        }
        "package" => {
            if let Some(text) = val.as_str() {
                spec.rename = Some(spec.name.clone());
                text.clone_into(&mut spec.name);
            }
        }
        _ => {}
    }
}

/// Write the generated aggregator crate to `out_dir`.
///
/// Recreates `Cargo.toml`, `build.rs`, `src/main.rs`, `src/tests.rs`,
/// and the `members/` bridge crates each call. Existing `target/` and
/// `Cargo.lock` artefacts are preserved.
///
/// # Errors
///
/// Returns an error when any filesystem write fails or when the
/// generated bridge / Cargo.toml content cannot be produced.
#[inline]
pub fn write_runner(plan: &Plan, out_dir: &Path) -> Result<()> {
    // Regenerate our own files from scratch. Preserve the aggregator's
    // `target/` directory across invocations — blowing it away on every
    // run would force a full recompile of the entire workspace every
    // time, and on macOS `remove_dir_all` additionally races with cargo
    // lock files still being written (ENOTEMPTY under concurrent IO).
    let src_dir = out_dir.join("src");
    for path in [
        out_dir.join("Cargo.toml"),
        out_dir.join("Cargo.lock"),
        out_dir.join("build.rs"),
    ] {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    if src_dir.exists() {
        fs::remove_dir_all(&src_dir).with_context(|| format!("removing {}", src_dir.display()))?;
    }
    let members_dir = out_dir.join("members");
    if members_dir.exists() {
        fs::remove_dir_all(&members_dir)
            .with_context(|| format!("removing {}", members_dir.display()))?;
    }
    fs::create_dir_all(&src_dir).with_context(|| format!("creating {}", src_dir.display()))?;

    for member in &plan.members {
        if !bridge_applies_to(member) {
            continue;
        }
        write_bridge_crate(plan, member, &members_dir).with_context(|| {
            format!("writing bridge crate for member `{}`", member.package_name)
        })?;
    }

    let cargo_toml = build_cargo_toml(plan)?;
    fs::write(out_dir.join("Cargo.toml"), cargo_toml)
        .with_context(|| format!("writing Cargo.toml under {}", out_dir.display()))?;

    fs::write(src_dir.join("main.rs"), build_main_rs(plan))
        .with_context(|| format!("writing src/main.rs under {}", out_dir.display()))?;

    fs::write(src_dir.join("tests.rs"), build_tests_rs(plan))
        .with_context(|| format!("writing src/tests.rs under {}", out_dir.display()))?;

    fs::write(out_dir.join("build.rs"), build_build_rs(plan))
        .with_context(|| format!("writing build.rs under {}", out_dir.display()))?;

    Ok(())
}

/// Two gates a member must pass to get a bridge crate generated.
///
/// It has to be a lib (bridges re-point `[lib] path`, nothing to
/// re-point for a bin-only crate) AND it has to have rudzio surface
/// inside `src/**` (otherwise nothing in the member's own compilation
/// unit needs dev-deps — integration tests in `tests/*.rs` are already
/// pulled into the aggregator's compilation unit instead and see the
/// aggregator's deps directly).
#[inline]
#[must_use]
pub const fn bridge_applies_to(member: &MemberPlan) -> bool {
    member.has_lib && member.has_src_rudzio_suite && member.src_lib_path.is_some()
}

/// Emit `<out>/<normalized>/Cargo.toml` for a member's bridge crate.
///
/// The bridge:
/// - declares `[package] name = "<member>_rudzio_bridge"` so cargo sees
///   it as a distinct crate (avoids collisions with the real member
///   that the IDE / `cargo test -p <member>` still target);
/// - declares `[lib] name = "<member>" path = "<abs>/src/lib.rs"` so
///   `extern crate <member>;` in the aggregator's `src/main.rs` resolves
///   to the bridge's rlib (renamed via `package =` in the aggregator's
///   `[dependencies]`) and the compilation unit is the REAL src tree;
/// - carries `build = "<abs>/build.rs"` when the real member has one,
///   so build-script env vars (e.g. `rustc-env=SOMETHING=...`) still
///   fire at the bridge's compilation;
/// - merges `[dependencies]` ∪ `[dev-dependencies]` ∪ the target-cfg
///   variants of both into a single `[dependencies]` table, so
///   `use ::rudzio::...` + any fake/mockito/serde-json etc. in src
///   tests resolve when cargo compiles under `--cfg rudzio_test`;
/// - carries an empty `[workspace]` stanza so the bridge becomes its
///   own workspace root (the enclosing rudzio-auto-runner aggregator is
///   also its own workspace, and cargo rejects nested workspaces that
///   share `[workspace]` tables).
///
/// The bridge dir itself does NOT get a `src/lib.rs` placeholder —
/// explicit `[lib] path = ...` tells cargo to look only at that path,
/// so an empty dir with just a `Cargo.toml` is enough.
///
/// # Errors
///
/// Returns an error when the bridge directory cannot be wiped /
/// recreated, when the bridge `Cargo.toml` / `build.rs` cannot be
/// emitted, or when symlinking the member tree fails.
#[inline]
pub fn write_bridge_crate(plan: &Plan, member: &MemberPlan, out: &Path) -> Result<()> {
    let bridge_dir = out.join(bridge_dir_name(member));
    // Wipe any pre-existing bridge directory so stale symlinks from a
    // prior run (member entry renamed or removed) don't survive.
    if bridge_dir.exists() {
        fs::remove_dir_all(&bridge_dir)
            .with_context(|| format!("wiping stale bridge dir {}", bridge_dir.display()))?;
    }
    fs::create_dir_all(&bridge_dir)
        .with_context(|| format!("creating bridge dir {}", bridge_dir.display()))?;
    let manifest = build_bridge_cargo_toml(plan, member)?;
    fs::write(bridge_dir.join("Cargo.toml"), manifest)
        .with_context(|| format!("writing bridge Cargo.toml at {}", bridge_dir.display()))?;
    fs::write(bridge_dir.join("build.rs"), build_bridge_build_rs(member))
        .with_context(|| format!("writing bridge build.rs at {}", bridge_dir.display()))?;
    symlink_member_tree_into_bridge(&member.manifest_dir, &bridge_dir)?;
    Ok(())
}

/// Symlink every non-skiplist top-level entry of the member's manifest
/// dir into the bridge dir. This makes `CARGO_MANIFEST_DIR`-relative
/// path lookups (e.g. `include_str!("data.json")`,
/// `sqlx::migrate!("migrations")`) resolve transparently under bridge
/// compile — the bridge dir becomes a structural twin of the member's
/// root, minus the Cargo.toml / build.rs that the bridge overrides.
///
/// If the member's `manifest_dir` is unreadable (e.g. synthetic test
/// fixtures using a fake path), the symlink pass is a no-op. Bridge
/// compile would fail anyway in that case — the no-op keeps synthetic
/// unit tests of Cargo.toml emission unaffected.
fn symlink_member_tree_into_bridge(member_dir: &Path, bridge_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(member_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if BRIDGE_SKIPLIST.contains(&name_str) {
            continue;
        }
        let source = entry.path();
        let target = bridge_dir.join(&name);
        create_symlink(&source, &target).with_context(|| {
            format!(
                "symlinking bridge entry {} -> {}",
                target.display(),
                source.display()
            )
        })?;
    }
    Ok(())
}

/// Create a filesystem symlink from `source` to `target` (Unix variant).
#[cfg(unix)]
fn create_symlink(source: &Path, target: &Path) -> IoResult<()> {
    use std::os::unix::fs::symlink;
    symlink(source, target)
}

/// Create a filesystem symlink from `source` to `target` (Windows stub).
#[cfg(windows)]
fn create_symlink(_source: &Path, _target: &Path) -> IoResult<()> {
    use std::io::{Error, ErrorKind};
    Err(Error::new(
        ErrorKind::Unsupported,
        "cargo-rudzio bridge symlinking is not yet implemented on Windows",
    ))
}

/// Synthesize the bridge's `build.rs`.
///
/// Every bridge gets one — it is the per-compile-unit emission site for
/// `cargo:rustc-cfg=rudzio_test` that scopes the cfg to the bridge's
/// rustc invocation only (as opposed to propagating it through ambient
/// `RUSTFLAGS`, which leaked into nested `cargo build --bins` and
/// caused thousands of unresolved-crate errors in downstream bin
/// crates).
///
/// For bin-bearing members the build.rs additionally invokes
/// `expose_member_bins` so `cargo:rustc-env=CARGO_BIN_EXE_<bin>=<abs>`
/// reaches the bridge's compile unit, where `rudzio::bin!(...)`
/// ultimately expands. The member's own `build.rs` is never forwarded:
/// its emissions would scope to the member's standalone compile unit,
/// but the cfg needs to land on the bridge.
#[inline]
#[must_use]
pub fn build_bridge_build_rs(member: &MemberPlan) -> String {
    let mut out = String::new();
    if member.bin_names.is_empty() {
        out.push_str("fn main() {\n");
        out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
        out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
        out.push_str("}\n");
        return out;
    }
    out.push_str(BUILD_RS_HELPERS);
    out.push_str("\nfn main() -> Result<(), String> {\n");
    out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
    out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
    let _ignored: FmtResult =write!(
        out,
        "    expose_member_bins({}, {}, &[",
        quote_str(&member.package_name),
        quote_str(&member.manifest_dir.to_string_lossy()),
    );
    for (idx, bin) in member.bin_names.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&quote_str(bin));
    }
    out.push_str("])?;\n    Ok(())\n}\n");
    out
}

/// Cargo `[package] name` to use for `member`'s bridge crate.
#[inline]
#[must_use]
pub fn bridge_package_name(member: &MemberPlan) -> String {
    format!(
        "{}_rudzio_bridge",
        crate_name_to_ident(&member.package_name)
    )
}

/// Subdirectory name (under the aggregator's `members/` dir) that holds the bridge for `member`.
#[inline]
#[must_use]
pub fn bridge_dir_name(member: &MemberPlan) -> String {
    crate_name_to_ident(&member.package_name)
}

/// Render the bridge crate's `Cargo.toml` for `member` as a TOML document string.
///
/// Mirrors the merged `[dependencies]` ∪ `[dev-dependencies]` of the
/// member, points `[lib] path` at the real `src/lib.rs`, and synthesises
/// a `build.rs` that emits `cargo:rustc-cfg=rudzio_test`.
///
/// # Errors
///
/// Returns an error when the member has no `src_lib_path`, or when
/// rendering any dependency entry into TOML fails.
#[inline]
pub fn build_bridge_cargo_toml(plan: &Plan, member: &MemberPlan) -> Result<String> {
    let lib_path = member.src_lib_path.as_ref().ok_or_else(|| {
        anyhow!(
            "bridge requested for `{}` but the member has no [lib] target",
            member.package_name
        )
    })?;

    let mut doc = DocumentMut::new();

    let mut pkg = Table::new();
    pkg.insert("name", value(bridge_package_name(member)));
    pkg.insert("version", value("0.0.0"));
    pkg.insert("edition", value(member.edition.as_str()));
    pkg.insert("publish", value(false));
    // Every bridge gets its own synthesised build.rs that emits
    // `cargo:rustc-cfg=rudzio_test` (and, for bin members, exposes the
    // member's bins via `expose_member_bins`). The member's own
    // build.rs is never forwarded — its emissions would scope to the
    // member's standalone compile unit, but the cfg needs to land on
    // the bridge's compile unit instead.
    pkg.insert("build", value("build.rs"));
    doc.insert("package", Item::Table(pkg));

    // No `[workspace]` stanza here: the bridge deliberately attaches to
    // the enclosing aggregator's workspace (which lists the bridge under
    // `[workspace] members`). Declaring a second `[workspace]` would
    // produce nested workspaces, which cargo rejects ("multiple
    // workspace roots found").

    let mut lib = Table::new();
    lib.insert("name", value(crate_name_to_ident(&member.package_name)));
    // Relative path; the bridge dir symlinks the member's top-level
    // entries, so resolving `<rel>/lib.rs` inside the bridge dir lands
    // on the member's real source. Path-based macros like
    // `include_str!` / `sqlx::migrate!` resolve against
    // `CARGO_MANIFEST_DIR` (= the bridge dir) and likewise reach the
    // member tree through the symlinks.
    let rel_lib = lib_path
        .strip_prefix(&member.manifest_dir).map_or_else(|_| PathBuf::from("src").join("lib.rs"), Path::to_path_buf);
    let rel_lib_str = rel_lib.to_string_lossy().replace('\\', "/");
    lib.insert("path", value(rel_lib_str));
    doc.insert("lib", Item::Table(lib));

    let deps_tbl = build_bridge_dependencies(plan, member)?;
    doc.insert("dependencies", Item::Table(deps_tbl));

    // No [build-dependencies]: the synth build.rs uses only std
    // (PathBuf, Command, std::env). External crates the member's
    // own build.rs may have needed are not relevant here because
    // we don't forward that build.rs.

    if let Some(features_tbl) = build_bridge_features(member) {
        doc.insert("features", Item::Table(features_tbl));
    }

    Ok(doc.to_string())
}

/// Build the bridge's `[features]` table. Mirrors the member's own
/// `[features]` 1:1, then overwrites the `default` entry with the
/// sorted, deduped union of (member's own `default`) ∪
/// (`rudzio_activated_features`). Returns `None` when both sources are
/// empty — no `[features]` section is emitted in that case.
fn build_bridge_features(member: &MemberPlan) -> Option<Table> {
    if member.features.is_empty() && member.rudzio_activated_features.is_empty() {
        return None;
    }

    let mut tbl = Table::new();
    for (key, values) in &member.features {
        if key == "default" {
            continue;
        }
        let mut arr = Array::new();
        for feat in values {
            arr.push(feat.as_str());
        }
        tbl.insert(key, value(arr));
    }

    let mut default: BTreeSet<String> = BTreeSet::new();
    if let Some(member_default) = member.features.get("default") {
        for feat in member_default {
            let _default_inserted: bool = default.insert(feat.clone());
        }
    }
    for feat in &member.rudzio_activated_features {
        let _default_inserted: bool = default.insert(feat.clone());
    }

    let mut default_arr = Array::new();
    for feat in &default {
        default_arr.push(feat.as_str());
    }
    tbl.insert("default", value(default_arr));

    Some(tbl)
}

/// Merge the member's `[dependencies]` + `[dev-dependencies]` + both
/// target-cfg variants into one flat `[dependencies]` table for the
/// bridge, plus inject `rudzio` if the member didn't already declare
/// it. Anyhow is intentionally NOT injected: rudzio's void-fn rewrite
/// uses `::rudzio::BoxError` (defined in rudzio itself) as the error
/// type, so no anyhow dependency leaks onto users through the bridge.
fn build_bridge_dependencies(plan: &Plan, member: &MemberPlan) -> Result<Table> {
    let mut deps = Table::new();

    let mut merged: BTreeMap<String, DevDepSpec> = BTreeMap::new();
    for dep in &member.dev_deps {
        // Skip self-references in the member's own [dev-dependencies]
        // (typical of dogfooding setups, e.g. rudzio's own root manifest
        // declares `rudzio = { path = "." }` to test itself). Without
        // this guard, the sibling-bridge redirect in `render_dev_dep`
        // would point the bridge at its own package and cargo halts with
        // a `cyclic package dependency` error. The rudzio-injection
        // block below fills the entry back in from the workspace spec.
        if dep.name == member.package_name {
            continue;
        }
        let entry_name = dep.rename.as_deref().unwrap_or(&dep.name).to_owned();
        merged
            .entry(entry_name)
            .and_modify(|existing| {
                for feat in &dep.features {
                    if !existing.features.contains(feat) {
                        existing.features.push(feat.clone());
                    }
                }
                existing.uses_default_features &= dep.uses_default_features;
            })
            .or_insert_with(|| dep.clone());
    }

    for (entry_name, dep) in &merged {
        let item = render_dev_dep(dep, plan)?;
        deps.insert(entry_name, item);
    }

    // Bridges exist to expose dev-deps under `--cfg rudzio_test`, and
    // rudzio itself is the universally-required one. If the member
    // declared rudzio only under `[dev-dependencies]` the merge above
    // already surfaces it; if the member didn't declare rudzio at all
    // (defensive — we already filter to rudzio-using members) we inject
    // the aggregator's unified spec so `use ::rudzio::*` still resolves.
    //
    // Skip the inject for rudzio's own bridge: a `rudzio` dep on the
    // rudzio bridge would either self-loop (after sibling-bridge
    // redirection) or pull in `/rudzio` as a second compile unit
    // alongside the bridge, leading to duplicate `#[distributed_slice]`
    // registrations at runtime. Rudzio's own src uses `crate::*` for
    // self-references; an extern `rudzio` crate isn't needed.
    if member.package_name != RUDZIO_DEP && !deps.contains_key(RUDZIO_DEP) {
        let tbl = build_rudzio_inline_table(&plan.rudzio_spec);
        deps.insert(RUDZIO_DEP, Item::Value(Value::InlineTable(tbl)));
    }

    Ok(deps)
}

/// Build the aggregator's `src/main.rs`.
///
/// Emits `extern crate <crate>;` for every member we also list as a
/// path dep in the aggregator's `[dependencies]`, which forces rustc
/// to actually link each member's rlib into the final binary. Without
/// this, rustc drops unreferenced rlibs during link-time DCE and
/// linkme's `#[link_section]` statics in those crates (even annotated
/// `#[used]`, which only blocks in-object DCE) never reach the
/// binary — meaning `#[rudzio::test]` fns under a member's `src/**`
/// wouldn't be discovered at run time. The member package name is
/// normalised hyphens→underscores to match cargo's own
/// crate-name-to-ident rule.
#[inline]
#[must_use]
pub fn build_main_rs(plan: &Plan) -> String {
    let mut out = String::from(
        "#![allow(
    unsafe_code,
    unreachable_pub,
    unused_crate_dependencies,
    unused_extern_crates,
    clippy::tests_outside_test_module,
    clippy::single_component_path_imports,
    reason = \"auto-generated rudzio test aggregator\"
)]

",
    );

    let workspace_root_abs = plan.workspace_root.as_std_path();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for member in &plan.members {
        if paths_equal(&member.manifest_dir, workspace_root_abs) {
            continue;
        }
        if !member.has_lib {
            continue;
        }
        let ident = crate_name_to_ident(&member.package_name);
        if !seen.insert(ident.clone()) {
            continue;
        }
        let _ignored: FmtResult =writeln!(out, "extern crate {ident};");
    }

    out.push_str(
        "
mod tests;

#[rudzio::main]
fn main() {}
",
    );
    out
}

/// Normalise a Cargo package name (which allows hyphens) into the Rust
/// identifier cargo actually uses for `extern crate` / path references.
fn crate_name_to_ident(name: &str) -> String {
    name.replace('-', "_")
}

/// Build the aggregator crate's `Cargo.toml` from the resolved `Plan`.
fn build_cargo_toml(plan: &Plan) -> Result<String> {
    let mut doc = DocumentMut::new();

    let mut pkg = Table::new();
    pkg.insert("name", value(AGGREGATOR_NAME));
    pkg.insert("version", value("0.0.0"));
    pkg.insert("edition", value("2024"));
    pkg.insert("publish", value(false));
    doc.insert("package", Item::Table(pkg));

    // `[workspace]` makes the aggregator its own workspace root so
    // cargo doesn't complain that its manifest is missing from the
    // enclosing rudzio workspace's `members` list (the aggregator lives
    // at `<target-dir>/rudzio-auto-runner`, outside that list). It also
    // insulates feature unification from the parent graph. The bin-only
    // workspace-member problem that cuts off `rudzio::build::expose_bins`
    // here is handled in `build.rs` — see `build_build_rs`.
    //
    // Bridge crates live at `members/<name>/Cargo.toml`. Cargo would
    // otherwise reject those as "non-member manifests nested inside a
    // workspace root". Listing them as explicit workspace members makes
    // cargo treat them as part of this workspace. If no member
    // qualifies for bridging, the `members` key is omitted entirely so
    // the aggregator stays a single-crate virtual workspace.
    let mut ws_tbl = Table::new();
    let mut members_arr = Array::new();
    for member in &plan.members {
        if !bridge_applies_to(member) {
            continue;
        }
        members_arr.push(Value::String(Formatted::new(format!(
            "members/{}",
            bridge_dir_name(member)
        ))));
    }
    if !members_arr.is_empty() {
        ws_tbl.insert("members", Item::Value(Value::Array(members_arr)));
    }
    doc.insert("workspace", Item::Table(ws_tbl));

    let mut bin = Table::new();
    bin.insert("name", value(AGGREGATOR_NAME));
    bin.insert("path", value("src/main.rs"));
    bin.insert("test", value(false));
    let mut bins = toml_edit::ArrayOfTables::new();
    bins.push(bin);
    doc.insert("bin", Item::ArrayOfTables(bins));

    let mut deps = Table::new();

    // The aggregator injects rudzio (directly, with the union of
    // requested runtime features). anyhow is NOT injected — the
    // migrator emits `::rudzio::BoxError` rather than `::anyhow::Result`
    // for void-fn rewrites, and user-written tests that use anyhow
    // declare it in their own `[dev-dependencies]`; those surface here
    // through the dev-dep union below.
    //
    // rudzio: derived from how workspace members declare it (path, git,
    // or version — never hardcoded to the workspace root, which is wrong
    // for downstream users whose workspace root is NOT rudzio). When
    // rudzio is itself a bridged workspace member (rudzio's own repo
    // dogfooding), route through that member's bridge so the aggregator
    // and the bridges link to a single rudzio rlib — otherwise the
    // linkme `#[distributed_slice]` machinery rudzio uses for test
    // discovery panics with `duplicate #[distributed_slice]` at startup
    // because two distinct rudzio compile units each register the
    // collection.
    deps.insert(
        "rudzio",
        Item::Value(Value::InlineTable(aggregator_rudzio_inline_table(plan))),
    );

    // Every rudzio-using member as a path dep with default features.
    // Skip the workspace root itself (already injected as
    // `rudzio = { path = ..., features = [...] }`) and bin-only members
    // (cargo rejects libless path deps; they participate via the
    // aggregator's `[workspace.members]` list so `expose_bins` can see
    // them in cargo-metadata output).
    //
    // Members that carry src-embedded rudzio suites go through a
    // generated bridge crate (see `bridge_applies_to`): instead of the
    // aggregator depending on the member's real manifest dir (which
    // only has `[dev-dependencies]` visible when cargo compiles the
    // member's own `[[test]]` target — NOT when the aggregator pulls
    // it in as a plain lib), the aggregator depends on the bridge at
    // `./members/<name>/` with `package = "<name>_rudzio_bridge"`. The
    // bridge re-points `[lib] path` at the real `src/lib.rs` but owns
    // the deps cargo sees, so `use ::rudzio::...` inside the member's
    // src tests compiles under `--cfg rudzio_test` without the member's
    // own Cargo.toml carrying any rudzio-specific machinery.
    let workspace_root_abs = plan.workspace_root.as_std_path();
    for member in &plan.members {
        if paths_equal(&member.manifest_dir, workspace_root_abs) {
            continue;
        }
        if !member.has_lib {
            continue;
        }
        let mut tbl = InlineTable::new();
        if bridge_applies_to(member) {
            tbl.insert(
                "path",
                Value::String(Formatted::new(format!(
                    "./members/{}",
                    bridge_dir_name(member)
                ))),
            );
            tbl.insert(
                "package",
                Value::String(Formatted::new(bridge_package_name(member))),
            );
        } else {
            tbl.insert(
                "path",
                Value::String(Formatted::new(
                    member.manifest_dir.to_string_lossy().into_owned(),
                )),
            );
        }
        deps.insert(&member.package_name, Item::Value(Value::InlineTable(tbl)));
    }

    // Union of deps (normal + dev) across all members. First spec
    // encountered wins on path/version/rename; features accumulate
    // across all appearances. Skip rudzio (injected above), the
    // aggregator itself, and intra-workspace sibling members (added
    // separately as path deps).
    let member_names: BTreeSet<String> = plan
        .members
        .iter()
        .map(|member| member.package_name.clone())
        .collect();
    let mut merged: BTreeMap<String, DevDepSpec> = BTreeMap::new();
    for member in &plan.members {
        for dep in &member.dev_deps {
            let entry_name = dep.rename.as_deref().unwrap_or(&dep.name).to_owned();
            if entry_name == RUDZIO_DEP || entry_name == AGGREGATOR_NAME {
                continue;
            }
            if member_names.contains(&entry_name) {
                continue;
            }
            merged
                .entry(entry_name)
                .and_modify(|existing| {
                    for feat in &dep.features {
                        if !existing.features.contains(feat) {
                            existing.features.push(feat.clone());
                        }
                    }
                    existing.uses_default_features &= dep.uses_default_features;
                })
                .or_insert_with(|| dep.clone());
        }
    }
    for (entry_name, dep) in &merged {
        let item = render_dev_dep(dep, plan)?;
        deps.insert(entry_name, item);
    }

    doc.insert("dependencies", Item::Table(deps));

    // No `[build-dependencies]` needed — the aggregator's `build.rs`
    // shells out to `cargo build --bins` directly (std-only) rather than
    // going through `rudzio::build::expose_bins`, which can't reach
    // bin-only workspace members when the aggregator is its own workspace.

    Ok(doc.to_string())
}

/// Render `dep` as a sibling-bridge dependency entry when its package
/// name matches another bridged member of `plan`.
///
/// Without this redirection, the bridge's `[dependencies]` would point
/// at the original member path/version while the aggregator's
/// `[dependencies]` points at the sibling's bridge — cargo would then
/// compile the same crate twice (different package IDs because the
/// paths differ) and the aggregator would drown in
/// `the trait bound X: Y is not satisfied` errors at link time.
///
/// Matches against `member.package_name` (cargo's `[package].name`) so
/// renamed deps (`alias = { package = "real-pkg", ... }`) still resolve
/// to the right sibling — `dep.name` is already the post-rename real
/// package name (see `apply_dev_dep_field`'s `package` branch).
/// Features and `default-features` propagate using the same merge rules
/// as the workspace-inherited / direct branches in `render_dev_dep`, so
/// feature flags reach the bridge's mirrored `[features]` table.
fn render_sibling_bridge_dep(dep: &DevDepSpec, plan: &Plan) -> Option<Item> {
    let sibling = plan
        .members
        .iter()
        .find(|member| member.package_name == dep.name && bridge_applies_to(member))?;

    let mut tbl = InlineTable::new();
    tbl.insert(
        "path",
        Value::String(Formatted::new(format!("../{}", bridge_dir_name(sibling)))),
    );
    tbl.insert(
        "package",
        Value::String(Formatted::new(bridge_package_name(sibling))),
    );

    let ws_entry = if dep.workspace_inherited {
        plan.workspace_deps.get(&dep.name)
    } else {
        None
    };
    let mut feats: Vec<String> = ws_entry
        .map(|ws| ws.features.clone())
        .unwrap_or_default();
    feats.extend(dep.features.iter().cloned());
    feats.sort();
    feats.dedup();
    if !feats.is_empty() {
        let mut arr = Array::new();
        for feat in feats {
            arr.push(Value::String(Formatted::new(feat)));
        }
        tbl.insert("features", Value::Array(arr));
    }

    let uses_defaults =
        ws_entry.is_none_or(|ws| ws.uses_default_features) && dep.uses_default_features;
    if !uses_defaults {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }

    if dep.optional {
        tbl.insert("optional", Value::Boolean(Formatted::new(true)));
    }

    Some(Item::Value(Value::InlineTable(tbl)))
}

/// Render a single `DevDepSpec` as a TOML inline table (or bare version string for trivial entries).
fn render_dev_dep(dep: &DevDepSpec, plan: &Plan) -> Result<Item> {
    if let Some(item) = render_sibling_bridge_dep(dep, plan) {
        return Ok(item);
    }

    let mut tbl = InlineTable::new();
    if dep.workspace_inherited {
        // Expand the workspace-inherited dev-dep to a concrete path/version
        // spec, merging the member's extra fields. The aggregator has its
        // own `[workspace]` stanza (empty), so `workspace = true` would not
        // resolve here.
        let ws = plan
            .workspace_deps
            .get(&dep.name)
            .ok_or_else(|| anyhow!("dev-dep `{}` says `workspace = true` but root Cargo.toml has no `[workspace.dependencies.{}]` entry", dep.name, dep.name))?;
        if let Some(path_buf) = &ws.path {
            tbl.insert(
                "path",
                Value::String(Formatted::new(path_buf.to_string_lossy().into_owned())),
            );
        } else if let Some(version) = &ws.version_req {
            tbl.insert("version", Value::String(Formatted::new(version.clone())));
        } else if let Some(url) = &ws.git {
            tbl.insert("git", Value::String(Formatted::new(url.clone())));
            match &ws.git_ref {
                Some(GitRef::Rev(rev)) => {
                    tbl.insert("rev", Value::String(Formatted::new(rev.clone())));
                }
                Some(GitRef::Branch(branch)) => {
                    tbl.insert("branch", Value::String(Formatted::new(branch.clone())));
                }
                Some(GitRef::Tag(tag)) => {
                    tbl.insert("tag", Value::String(Formatted::new(tag.clone())));
                }
                None => {}
            }
        } else {
            bail!(
                "workspace dep `{}` has neither `path`, `version`, nor `git` to inherit",
                dep.name
            );
        }
        let mut feats: Vec<String> = ws.features.clone();
        feats.extend(dep.features.iter().cloned());
        feats.sort();
        feats.dedup();
        if !feats.is_empty() {
            let mut arr = Array::new();
            for feat in feats {
                arr.push(Value::String(Formatted::new(feat)));
            }
            tbl.insert("features", Value::Array(arr));
        }
        let dflt = dep.uses_default_features && ws.uses_default_features;
        if !dflt {
            tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
        }
    } else if let Some(path_buf) = &dep.path {
        tbl.insert(
            "path",
            Value::String(Formatted::new(path_buf.to_string_lossy().into_owned())),
        );
        if !dep.features.is_empty() {
            let mut arr = Array::new();
            for feat in &dep.features {
                arr.push(Value::String(Formatted::new(feat.clone())));
            }
            tbl.insert("features", Value::Array(arr));
        }
        if !dep.uses_default_features {
            tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
        }
    } else if dep.features.is_empty()
        && dep.uses_default_features
        && dep.rename.is_none()
        && !dep.optional
    {
        return Ok(Item::Value(Value::String(Formatted::new(
            dep.version_req.clone(),
        ))));
    } else {
        tbl.insert(
            "version",
            Value::String(Formatted::new(dep.version_req.clone())),
        );
        if !dep.features.is_empty() {
            let mut arr = Array::new();
            for feat in &dep.features {
                arr.push(Value::String(Formatted::new(feat.clone())));
            }
            tbl.insert("features", Value::Array(arr));
        }
        if !dep.uses_default_features {
            tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
        }
    }
    if dep.rename.is_some() {
        tbl.insert("package", Value::String(Formatted::new(dep.name.clone())));
    }
    if dep.optional {
        tbl.insert("optional", Value::Boolean(Formatted::new(true)));
    }
    Ok(Item::Value(Value::InlineTable(tbl)))
}

/// Build the aggregator's `src/tests.rs` body — `#[path]`-included submodules per member's tests.
fn build_tests_rs(plan: &Plan) -> String {
    // Per-crate submodule namespacing. Each member's tests/*.rs files
    // nest under `mod <crate_name> { ... }` so sibling helper modules
    // resolve via `use super::helper::*` from inside a test file —
    // that path works BOTH in per-crate `cargo test -p X` (where the
    // test binary's crate root has `mod helper;` and `mod test_file;`
    // as peers, so super from inside test_file = crate root = has
    // helper) AND in the aggregator (super from tests::X::test_file =
    // tests::X = has mod helper). Flat prefix-mangling (the previous
    // layout) broke the former because `crate::helper` resolved to
    // the aggregator root, which doesn't have helper under its short
    // name. Per-crate nesting also eliminates cross-crate name
    // collisions without needing a prefix at all.
    let mut per_crate_mod_names: BTreeSet<String> = BTreeSet::new();
    let mut out = String::new();
    for member in &plan.members {
        if member.test_files.is_empty() {
            continue;
        }
        let base_ident = sanitize_ident(&member.package_name);
        let mut crate_mod = base_ident.clone();
        let mut dedup_counter = 1_u32;
        while !per_crate_mod_names.insert(crate_mod.clone()) {
            crate_mod = format!("{base_ident}_{dedup_counter}");
            dedup_counter = dedup_counter.saturating_add(1);
        }
        let _crate_header: FmtResult = writeln!(out, "mod {crate_mod} {{");

        let mut used_inner: BTreeSet<String> = BTreeSet::new();
        for file in &member.test_files {
            let stem = file
                .file_stem()
                .and_then(|os_stem| os_stem.to_str())
                .unwrap_or("test");
            let stem_ident = sanitize_ident(stem);
            let mut inner = stem_ident.clone();
            let mut inner_counter = 1_u32;
            while !used_inner.insert(inner.clone()) {
                inner = format!("{stem_ident}_{inner_counter}");
                inner_counter = inner_counter.saturating_add(1);
            }
            let _path_line: FmtResult = writeln!(
                out,
                "    #[path = {}]\n    mod {inner};",
                quote_str(&file.to_string_lossy())
            );
        }
        out.push_str("}\n");
    }
    out
}

/// Build the aggregator's `build.rs` content — emits `cargo:rustc-cfg=rudzio_test` and bin export shims.
fn build_build_rs(plan: &Plan) -> String {
    let bin_members: Vec<&MemberPlan> = plan
        .members
        .iter()
        .filter(|member| !member.bin_names.is_empty())
        .collect();
    // The aggregator always emits `cargo:rustc-cfg=rudzio_test`: this is
    // where the cfg enters the aggregator's own compile unit. Cargo
    // scopes `cargo:rustc-cfg=` per-crate, so the emission here does NOT
    // leak into nested `cargo build --bins` invocations that
    // `expose_member_bins` may trigger below — each of those gets its
    // own untouched rustc.
    //
    // For bin members, shell out to `cargo build --bins --manifest-path
    // <Cargo.toml>` into a sandboxed OUT_DIR target, then emit
    // `cargo:rustc-env=CARGO_BIN_EXE_<bin>=<abs>` for each so that
    // `env!(CARGO_BIN_EXE_<name>)` in `#[path]`-included integration
    // sources resolves at compile time. (The aggregator has its own
    // `[workspace]` stanza, so `rudzio::build::expose_bins`'s
    // metadata-based approach isn't applicable here.)
    let mut out = String::new();
    if bin_members.is_empty() {
        out.push_str("fn main() {\n");
        out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
        out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
        out.push_str("}\n");
        return out;
    }
    out.push_str(BUILD_RS_HELPERS);
    out.push_str("\nfn main() -> Result<(), String> {\n");
    out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
    out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
    for member in bin_members {
        let _ignored: FmtResult =write!(
            out,
            "    expose_member_bins({}, {}, &[",
            quote_str(&member.package_name),
            quote_str(&member.manifest_dir.to_string_lossy()),
        );
        for (idx, bin) in member.bin_names.iter().enumerate() {
            if idx > 0 {
                out.push_str(", ");
            }
            out.push_str(&quote_str(bin));
        }
        out.push_str("])?;\n");
    }
    out.push_str("    Ok(())\n}\n");
    out
}

/// Replace any non `[a-zA-Z0-9_]` chars in `value` with `_`, prefixing a digit-starter with `_`.
fn sanitize_ident(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.chars().next().is_some_and(|first| first.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Render `value` as a Rust-source `&str` literal (with surrounding double-quotes and escapes).
///
/// Used by emitters of generated Rust source to embed dynamic strings
/// safely into the output.
fn quote_str(value: &str) -> String {
    let mut buf = String::with_capacity(value.len().saturating_add(2));
    buf.push('"');
    for character in value.chars() {
        match character {
            '\\' => buf.push_str("\\\\"),
            '"' => buf.push_str("\\\""),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            '\0' => buf.push_str("\\0"),
            other if u32::from(other) < 0x20 || other == '\u{7f}' => {
                let _ignored: FmtResult = write!(buf, "\\u{{{:x}}}", u32::from(other));
            }
            other => buf.push(other),
        }
    }
    buf.push('"');
    buf
}

/// Two paths refer to the same canonicalized location.
///
/// Falls back to direct equality when canonicalization fails (the paths
/// may not exist on disk yet — e.g. synthetic test fixtures).
fn paths_equal(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .ok()
        .zip(fs::canonicalize(right).ok())
        .is_some_and(|(canonical_left, canonical_right)| canonical_left == canonical_right)
        || left == right
}

/// Build a `Plan` by querying `cargo metadata` from the current working directory.
///
/// # Errors
///
/// Returns an error when `cargo metadata` fails or when reconciling
/// member metadata into a `Plan` fails.
#[inline]
pub fn plan_from_cwd() -> Result<Plan> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("failed to run `cargo metadata --no-deps` from the current directory")?;
    build_plan(&metadata)
}
