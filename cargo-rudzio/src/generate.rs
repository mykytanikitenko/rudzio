use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use cargo_metadata::{
    Metadata, MetadataCommand, Package, TargetKind, camino::Utf8PathBuf,
};
use toml_edit::{Array, DocumentMut, Formatted, InlineTable, Item, Table, Value, value};

const RUDZIO_DEP: &str = "rudzio";
const AGGREGATOR_NAME: &str = "rudzio-auto-runner";

/// Everything extracted from `cargo metadata` plus the workspace root's
/// Cargo.toml that the generator needs to emit the aggregator.
pub struct Plan {
    pub workspace_root: Utf8PathBuf,
    pub target_directory: Utf8PathBuf,
    /// Members of the workspace that actually depend on rudzio AND
    /// contribute at least one integration-test source file.
    pub members: Vec<MemberPlan>,
    /// Resolved `rudzio` dependency to emit in the aggregator's
    /// `[dependencies]` table — derived from how the workspace's own
    /// members declare rudzio (path / git / version), with features
    /// unioned across every member's declaration plus `common` + `build`.
    pub rudzio_spec: RudzioSpec,
    /// Path → version overrides keyed by dep name, pulled from the
    /// workspace root's `[workspace.dependencies]` table. Used when a
    /// member's dev-dep entry says `workspace = true`.
    pub workspace_deps: BTreeMap<String, WorkspaceDepSpec>,
}

pub struct MemberPlan {
    pub package_name: String,
    pub manifest_dir: PathBuf,
    /// Absolute paths to the member's integration-test source files
    /// (excludes its `tests/main.rs` shim).
    pub test_files: Vec<PathBuf>,
    /// Names of the member's `[[bin]]` targets.
    pub bin_names: Vec<String>,
    /// `true` iff the member declares a `[lib]` target (or has the
    /// implicit `src/lib.rs`). Bin-only crates can't be listed as
    /// regular `[dependencies]` entries — they go into
    /// `[workspace.members]` instead.
    pub has_lib: bool,
    /// Dev-dep entries the aggregator must re-emit so the pulled-in
    /// test sources compile.
    pub dev_deps: Vec<DevDepSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitRef {
    Rev(String),
    Branch(String),
    Tag(String),
}

#[derive(Clone)]
pub struct DevDepSpec {
    pub name: String,
    pub rename: Option<String>,
    pub version_req: String,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub git_ref: Option<GitRef>,
    pub features: Vec<String>,
    pub uses_default_features: bool,
    /// Raw spec from the member's Cargo.toml, used when it says
    /// `workspace = true` and we need to defer to the workspace entry.
    pub workspace_inherited: bool,
}

#[derive(Clone)]
pub struct WorkspaceDepSpec {
    pub version_req: Option<String>,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub git_ref: Option<GitRef>,
    pub features: Vec<String>,
    pub uses_default_features: bool,
}

/// Resolved location for the `rudzio` dependency that the aggregator
/// should emit. Mirrors the three mutually exclusive ways cargo lets a
/// crate reference another: a local path, a git URL, or a registry
/// version requirement.
#[derive(Clone, Debug)]
pub enum RudzioLocation {
    Path(PathBuf),
    Git { url: String, reference: Option<GitRef> },
    Version(String),
}

#[derive(Clone, Debug)]
pub struct RudzioSpec {
    pub location: RudzioLocation,
    pub features: Vec<String>,
    pub uses_default_features: bool,
}

pub fn plan_from_cwd() -> Result<Plan> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("failed to run `cargo metadata --no-deps` from the current directory")?;
    build_plan(&metadata)
}

impl Plan {
    pub fn default_output_dir(&self) -> PathBuf {
        PathBuf::from(self.target_directory.as_std_path()).join(AGGREGATOR_NAME)
    }
}

fn build_plan(metadata: &Metadata) -> Result<Plan> {
    let workspace_root_std = metadata.workspace_root.as_std_path();
    let workspace_deps = read_workspace_deps(workspace_root_std)
        .context("reading workspace root Cargo.toml")?;

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
            .filter(|d| d.name == RUDZIO_DEP)
            .collect();
        if rudzio_deps.is_empty() {
            continue;
        }
        for rdep in &rudzio_deps {
            for feat in &rdep.features {
                if feat.starts_with("runtime-") {
                    let _ = runtime_features.insert(feat.clone());
                }
            }
        }

        let manifest_dir = pkg
            .manifest_path
            .parent()
            .ok_or_else(|| anyhow!("package {} has no parent directory", pkg.name))?
            .to_path_buf();
        let manifest_dir_std = manifest_dir.as_std_path().to_path_buf();

        let exclude_list = load_rudzio_exclude_list(pkg.manifest_path.as_std_path())
            .with_context(|| {
                format!(
                    "loading `[package.metadata.rudzio].exclude` from {}",
                    pkg.manifest_path.as_std_path().display()
                )
            })?;

        let test_files = discover_test_files(pkg, &manifest_dir_std, &exclude_list)
            .with_context(|| format!("discovering test files for `{}`", pkg.name))?;
        if test_files.is_empty() {
            continue;
        }

        let bin_names: Vec<String> = pkg
            .targets
            .iter()
            .filter(|t| t.kind.iter().any(|k| matches!(k, TargetKind::Bin)))
            .map(|t| t.name.clone())
            .collect();

        let has_lib = pkg
            .targets
            .iter()
            .any(|t| t.kind.iter().any(|k| matches!(k, TargetKind::Lib)));

        let dev_deps =
            read_dev_deps(pkg.manifest_path.as_std_path()).with_context(|| {
                format!(
                    "reading dev-deps from {}",
                    pkg.manifest_path.as_std_path().display()
                )
            })?;

        members.push(MemberPlan {
            package_name: pkg.name.clone(),
            manifest_dir: manifest_dir_std,
            test_files,
            bin_names,
            has_lib,
            dev_deps,
        });
    }

    members.sort_by(|a, b| a.package_name.cmp(&b.package_name));

    let rudzio_spec = collect_rudzio_spec(&members, &workspace_deps, workspace_root_std)
        .context("deriving rudzio dependency spec for the aggregator")?;
    let rudzio_spec = inject_required_features(rudzio_spec, &runtime_features);

    Ok(Plan {
        workspace_root: metadata.workspace_root.clone(),
        target_directory: metadata.target_directory.clone(),
        members,
        rudzio_spec,
        workspace_deps,
    })
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
    let _ = feats.insert("common".to_owned());
    let _ = feats.insert("build".to_owned());
    for f in runtime_features {
        let _ = feats.insert(f.clone());
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
fn collect_rudzio_spec(
    members: &[MemberPlan],
    workspace_deps: &BTreeMap<String, WorkspaceDepSpec>,
    workspace_root: &Path,
) -> Result<RudzioSpec> {
    struct Resolved {
        member: String,
        path: Option<PathBuf>,
        git: Option<String>,
        git_ref: Option<GitRef>,
        version_req: Option<String>,
        features: Vec<String>,
        uses_default_features: bool,
    }

    let mut resolved: Vec<Resolved> = Vec::new();
    for member in members {
        for dd in &member.dev_deps {
            // `dd.name` is the package name post-rename. Rudzio is only
            // ever referenced by its own crate name (no rename), so a
            // straightforward name match is correct.
            if dd.name != RUDZIO_DEP {
                continue;
            }
            if dd.workspace_inherited {
                let ws = workspace_deps.get(RUDZIO_DEP).ok_or_else(|| {
                    anyhow!(
                        "member `{}` declares `rudzio = {{ workspace = true }}` but the workspace root has no `[workspace.dependencies.rudzio]` entry",
                        member.package_name
                    )
                })?;
                let mut feats: Vec<String> = ws.features.clone();
                feats.extend(dd.features.iter().cloned());
                resolved.push(Resolved {
                    member: member.package_name.clone(),
                    path: ws.path.clone(),
                    git: ws.git.clone(),
                    git_ref: ws.git_ref.clone(),
                    version_req: ws.version_req.clone(),
                    features: feats,
                    uses_default_features: dd.uses_default_features
                        && ws.uses_default_features,
                });
            } else {
                let version_req = if dd.version_req.is_empty() {
                    None
                } else {
                    Some(dd.version_req.clone())
                };
                resolved.push(Resolved {
                    member: member.package_name.clone(),
                    path: dd.path.clone(),
                    git: dd.git.clone(),
                    git_ref: dd.git_ref.clone(),
                    version_req,
                    features: dd.features.clone(),
                    uses_default_features: dd.uses_default_features,
                });
            }
        }
    }

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
    let path_holder = resolved.iter().find(|r| r.path.is_some());
    let git_holder = resolved.iter().find(|r| r.git.is_some());
    if let (Some(p), Some(g)) = (path_holder, git_holder) {
        bail!(
            "rudzio is declared inconsistently across workspace members: `{}` uses path={}, `{}` uses git={} — aggregator can't unify these",
            p.member,
            p.path.as_ref().map(|x| x.display().to_string()).unwrap_or_default(),
            g.member,
            g.git.clone().unwrap_or_default(),
        );
    }

    // Pick location. Path > git > version. (Path beats version because
    // it pins to a specific local checkout; git beats version because
    // it pins to a specific revision.)
    let location = if let Some(p) = path_holder.and_then(|r| r.path.clone()) {
        // Canonicalize so `.` / `..` segments inherited from member
        // manifests like `path = "."` don't leak into the aggregator's
        // emitted spec (cosmetic — cargo accepts both, but matching the
        // workspace_root form keeps regenerated output stable).
        let normalized = fs::canonicalize(&p).unwrap_or(p);
        RudzioLocation::Path(normalized)
    } else if let Some(url) = git_holder.and_then(|r| r.git.clone()) {
        // For the git ref, take the first non-None ref encountered (git
        // declarations without `rev`/`branch`/`tag` mean "default branch",
        // which we encode as None — cargo treats absence the same way).
        let reference = resolved.iter().find_map(|r| r.git_ref.clone());
        RudzioLocation::Git { url, reference }
    } else {
        // No path or git: every declaration is version-only (or
        // workspace-inherited from a version-only entry). Take the
        // first non-None version_req.
        let version = resolved
            .iter()
            .find_map(|r| r.version_req.clone())
            .ok_or_else(|| {
                anyhow!(
                    "rudzio is declared by workspace members but none of the declarations carry `path`, `git`, or `version` — aggregator can't reference rudzio"
                )
            })?;
        RudzioLocation::Version(version)
    };

    let mut features: BTreeSet<String> = BTreeSet::new();
    let mut uses_default_features = true;
    for r in &resolved {
        for f in &r.features {
            let _ = features.insert(f.clone());
        }
        uses_default_features &= r.uses_default_features;
    }

    Ok(RudzioSpec {
        location,
        features: features.into_iter().collect(),
        uses_default_features,
    })
}

/// Render a `RudzioSpec` as a Cargo dependency inline table. Emits the
/// minimal correct shape: location keys (`path` / `git` + ref / `version`)
/// followed by `features` (only when non-empty) and
/// `default-features = false` (only when the user opted out).
fn build_rudzio_inline_table(spec: &RudzioSpec) -> InlineTable {
    let mut tbl = InlineTable::new();
    match &spec.location {
        RudzioLocation::Path(p) => {
            tbl.insert(
                "path",
                Value::String(Formatted::new(p.to_string_lossy().into_owned())),
            );
        }
        RudzioLocation::Git { url, reference } => {
            tbl.insert("git", Value::String(Formatted::new(url.clone())));
            match reference {
                Some(GitRef::Rev(rev)) => {
                    tbl.insert("rev", Value::String(Formatted::new(rev.clone())));
                }
                Some(GitRef::Branch(branch)) => {
                    tbl.insert(
                        "branch",
                        Value::String(Formatted::new(branch.clone())),
                    );
                }
                Some(GitRef::Tag(tag)) => {
                    tbl.insert("tag", Value::String(Formatted::new(tag.clone())));
                }
                None => {}
            }
        }
        RudzioLocation::Version(v) => {
            tbl.insert("version", Value::String(Formatted::new(v.clone())));
        }
    }
    if !spec.features.is_empty() {
        let mut feats = Array::new();
        for f in &spec.features {
            feats.push(Value::String(Formatted::new(f.clone())));
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
        .filter(|t| t.kind.iter().any(|k| matches!(k, TargetKind::Test)))
        .map(|t| t.src_path.as_std_path().to_path_buf())
        .filter(|p| p != &main_shim)
        .collect();

    let mut files: Vec<PathBuf> = if explicit.is_empty() {
        if !tests_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(&tests_dir)
            .with_context(|| format!("reading {}", tests_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
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
        let s = entry.as_str().ok_or_else(|| {
            anyhow!(
                "`[package.metadata.rudzio].exclude` in {} must contain only strings",
                manifest_path.display()
            )
        })?;
        out.push(manifest_dir.join(s));
    }
    Ok(out)
}

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
    for (name, item) in deps.iter() {
        let spec = extract_workspace_dep_spec(item, workspace_root);
        drop(out.insert(name.to_owned(), spec));
    }
    Ok(out)
}

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
        Item::Value(Value::String(s)) => {
            spec.version_req = Some(s.value().to_owned());
        }
        Item::Value(Value::InlineTable(t)) => {
            for (k, v) in t.iter() {
                apply_ws_dep_field(&mut spec, k, v, workspace_root);
            }
        }
        Item::Table(t) => {
            for (k, v) in t.iter() {
                if let Some(val) = v.as_value() {
                    apply_ws_dep_field(&mut spec, k, val, workspace_root);
                }
            }
        }
        _ => {}
    }
    spec
}

fn apply_ws_dep_field(spec: &mut WorkspaceDepSpec, key: &str, val: &Value, base: &Path) {
    match key {
        "version" => {
            if let Some(s) = val.as_str() {
                spec.version_req = Some(s.to_owned());
            }
        }
        "path" => {
            if let Some(s) = val.as_str() {
                spec.path = Some(base.join(s));
            }
        }
        "git" => {
            if let Some(s) = val.as_str() {
                spec.git = Some(s.to_owned());
            }
        }
        "rev" => {
            if let Some(s) = val.as_str() {
                spec.git_ref = Some(GitRef::Rev(s.to_owned()));
            }
        }
        "branch" => {
            if let Some(s) = val.as_str() {
                spec.git_ref = Some(GitRef::Branch(s.to_owned()));
            }
        }
        "tag" => {
            if let Some(s) = val.as_str() {
                spec.git_ref = Some(GitRef::Tag(s.to_owned()));
            }
        }
        "features" => {
            if let Some(arr) = val.as_array() {
                spec.features = arr
                    .iter()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect();
            }
        }
        "default-features" => {
            if let Some(b) = val.as_bool() {
                spec.uses_default_features = b;
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
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent"))?;

    let mut out: Vec<DevDepSpec> = Vec::new();
    for section in ["dependencies", "dev-dependencies"] {
        collect_dev_deps(&doc, &[section], manifest_dir, &mut out);
    }

    if let Some(target_tbl) = doc.get("target").and_then(Item::as_table) {
        for (_cfg, cfg_item) in target_tbl.iter() {
            let Some(cfg_tbl) = cfg_item.as_table() else {
                continue;
            };
            for section in ["dependencies", "dev-dependencies"] {
                if let Some(Item::Table(deps_tbl)) = cfg_tbl.get(section) {
                    for (name, item) in deps_tbl.iter() {
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
    let Some(deps_tbl) = cur.as_table() else { return };
    for (name, item) in deps_tbl.iter() {
        if let Some(spec) = parse_dev_dep_entry(name, item, manifest_dir) {
            out.push(spec);
        }
    }
}

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
    };
    match item {
        Item::Value(Value::String(s)) => {
            spec.version_req = s.value().to_owned();
            Some(spec)
        }
        Item::Value(Value::InlineTable(t)) => {
            fill_dev_dep_from_pairs(&mut spec, t.iter(), manifest_dir);
            Some(spec)
        }
        Item::Table(t) => {
            fill_dev_dep_from_table(&mut spec, t, manifest_dir);
            Some(spec)
        }
        _ => None,
    }
}

fn fill_dev_dep_from_pairs<'a>(
    spec: &mut DevDepSpec,
    entries: impl Iterator<Item = (&'a str, &'a Value)>,
    manifest_dir: &Path,
) {
    for (k, v) in entries {
        apply_dev_dep_field(spec, k, v, manifest_dir);
    }
}

fn fill_dev_dep_from_table(spec: &mut DevDepSpec, t: &Table, manifest_dir: &Path) {
    for (k, v) in t.iter() {
        if let Some(val) = v.as_value() {
            apply_dev_dep_field(spec, k, val, manifest_dir);
        }
    }
}

fn apply_dev_dep_field(spec: &mut DevDepSpec, key: &str, val: &Value, manifest_dir: &Path) {
    match key {
        "workspace" => {
            if val.as_bool() == Some(true) {
                spec.workspace_inherited = true;
            }
        }
        "version" => {
            if let Some(s) = val.as_str() {
                spec.version_req = s.to_owned();
            }
        }
        "path" => {
            if let Some(s) = val.as_str() {
                spec.path = Some(manifest_dir.join(s));
            }
        }
        "git" => {
            if let Some(s) = val.as_str() {
                spec.git = Some(s.to_owned());
            }
        }
        "rev" => {
            if let Some(s) = val.as_str() {
                spec.git_ref = Some(GitRef::Rev(s.to_owned()));
            }
        }
        "branch" => {
            if let Some(s) = val.as_str() {
                spec.git_ref = Some(GitRef::Branch(s.to_owned()));
            }
        }
        "tag" => {
            if let Some(s) = val.as_str() {
                spec.git_ref = Some(GitRef::Tag(s.to_owned()));
            }
        }
        "features" => {
            if let Some(arr) = val.as_array() {
                spec.features = arr
                    .iter()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect();
            }
        }
        "default-features" => {
            if let Some(b) = val.as_bool() {
                spec.uses_default_features = b;
            }
        }
        "package" => {
            if let Some(s) = val.as_str() {
                spec.rename = Some(spec.name.clone());
                spec.name = s.to_owned();
            }
        }
        _ => {}
    }
}

pub fn write_runner(plan: &Plan, out_dir: &Path) -> Result<()> {
    // Regenerate from scratch every call.
    if out_dir.exists() {
        fs::remove_dir_all(out_dir)
            .with_context(|| format!("removing {}", out_dir.display()))?;
    }
    let src_dir = out_dir.join("src");
    fs::create_dir_all(&src_dir).with_context(|| format!("creating {}", src_dir.display()))?;

    let cargo_toml = build_cargo_toml(plan)?;
    fs::write(out_dir.join("Cargo.toml"), cargo_toml)
        .with_context(|| format!("writing Cargo.toml under {}", out_dir.display()))?;

    fs::write(src_dir.join("main.rs"), MAIN_RS)
        .with_context(|| format!("writing src/main.rs under {}", out_dir.display()))?;

    fs::write(src_dir.join("tests.rs"), build_tests_rs(plan))
        .with_context(|| format!("writing src/tests.rs under {}", out_dir.display()))?;

    fs::write(out_dir.join("build.rs"), build_build_rs(plan))
        .with_context(|| format!("writing build.rs under {}", out_dir.display()))?;

    Ok(())
}

const MAIN_RS: &str = "#![allow(
    unsafe_code,
    unreachable_pub,
    unused_crate_dependencies,
    clippy::tests_outside_test_module,
    reason = \"auto-generated rudzio test aggregator\"
)]

mod tests;

#[rudzio::main]
fn main() {}
";

fn build_cargo_toml(plan: &Plan) -> Result<String> {
    let mut doc = DocumentMut::new();

    let mut pkg = Table::new();
    pkg.insert("name", value(AGGREGATOR_NAME));
    pkg.insert("version", value("0.0.0"));
    pkg.insert("edition", value("2024"));
    pkg.insert("publish", value(false));
    doc.insert("package", Item::Table(pkg));

    // Empty `[workspace]` makes the aggregator its own workspace root
    // so cargo doesn't complain that its manifest is missing from the
    // enclosing rudzio workspace's `members` list (the aggregator lives
    // at `<target-dir>/rudzio-auto-runner`, outside that list). It also
    // insulates feature unification from the parent graph. The bin-only
    // workspace-member problem that cuts off `rudzio::build::expose_bins`
    // here is handled in `build.rs` — see `build_build_rs`.
    doc.insert("workspace", Item::Table(Table::new()));

    let mut bin = Table::new();
    bin.insert("name", value(AGGREGATOR_NAME));
    bin.insert("path", value("src/main.rs"));
    bin.insert("test", value(false));
    let mut bins = toml_edit::ArrayOfTables::new();
    bins.push(bin);
    doc.insert("bin", Item::ArrayOfTables(bins));

    let mut deps = Table::new();

    // anyhow: pin to workspace version if available, else "1".
    let anyhow_req = plan
        .workspace_deps
        .get("anyhow")
        .and_then(|s| s.version_req.clone())
        .unwrap_or_else(|| "1".to_owned());
    deps.insert("anyhow", value(anyhow_req));

    // rudzio: derived from how workspace members declare it (path, git,
    // or version — never hardcoded to the workspace root, which is wrong
    // for downstream users whose workspace root is NOT rudzio).
    {
        let tbl = build_rudzio_inline_table(&plan.rudzio_spec);
        deps.insert("rudzio", Item::Value(Value::InlineTable(tbl)));
    }

    // Every rudzio-using member as a path dep with default features.
    // Skip the workspace root itself (already injected as
    // `rudzio = { path = ..., features = [...] }`) and bin-only members
    // (cargo rejects libless path deps; they participate via the
    // aggregator's `[workspace.members]` list so `expose_bins` can see
    // them in cargo-metadata output).
    let workspace_root_abs = plan.workspace_root.as_std_path();
    for member in &plan.members {
        if paths_equal(&member.manifest_dir, workspace_root_abs) {
            continue;
        }
        if !member.has_lib {
            continue;
        }
        let mut tbl = InlineTable::new();
        tbl.insert(
            "path",
            Value::String(Formatted::new(
                member.manifest_dir.to_string_lossy().into_owned(),
            )),
        );
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
        .map(|m| m.package_name.clone())
        .collect();
    let mut merged: BTreeMap<String, DevDepSpec> = BTreeMap::new();
    for member in &plan.members {
        for dd in &member.dev_deps {
            let entry_name = dd.rename.as_deref().unwrap_or(&dd.name).to_owned();
            if entry_name == RUDZIO_DEP || entry_name == AGGREGATOR_NAME {
                continue;
            }
            if member_names.contains(&entry_name) {
                continue;
            }
            merged
                .entry(entry_name)
                .and_modify(|existing| {
                    for f in &dd.features {
                        if !existing.features.contains(f) {
                            existing.features.push(f.clone());
                        }
                    }
                    existing.uses_default_features &= dd.uses_default_features;
                })
                .or_insert_with(|| dd.clone());
        }
    }
    for (entry_name, dd) in &merged {
        let item = render_dev_dep(dd, plan)?;
        deps.insert(entry_name, item);
    }

    doc.insert("dependencies", Item::Table(deps));

    // No `[build-dependencies]` needed — the aggregator's `build.rs`
    // shells out to `cargo build --bins` directly (std-only) rather than
    // going through `rudzio::build::expose_bins`, which can't reach
    // bin-only workspace members when the aggregator is its own workspace.

    Ok(doc.to_string())
}

fn render_dev_dep(dd: &DevDepSpec, plan: &Plan) -> Result<Item> {
    let mut tbl = InlineTable::new();
    if dd.workspace_inherited {
        // Expand the workspace-inherited dev-dep to a concrete path/version
        // spec, merging the member's extra fields. The aggregator has its
        // own `[workspace]` stanza (empty), so `workspace = true` would not
        // resolve here.
        let ws = plan
            .workspace_deps
            .get(&dd.name)
            .ok_or_else(|| anyhow!("dev-dep `{}` says `workspace = true` but root Cargo.toml has no `[workspace.dependencies.{}]` entry", dd.name, dd.name))?;
        if let Some(p) = &ws.path {
            tbl.insert(
                "path",
                Value::String(Formatted::new(p.to_string_lossy().into_owned())),
            );
        } else if let Some(v) = &ws.version_req {
            tbl.insert("version", Value::String(Formatted::new(v.clone())));
        } else {
            bail!(
                "workspace dep `{}` has neither `path` nor `version` to inherit",
                dd.name
            );
        }
        let mut feats: Vec<String> = ws.features.clone();
        feats.extend(dd.features.iter().cloned());
        feats.sort();
        feats.dedup();
        if !feats.is_empty() {
            let mut arr = Array::new();
            for f in feats {
                arr.push(Value::String(Formatted::new(f)));
            }
            tbl.insert("features", Value::Array(arr));
        }
        let dflt = dd.uses_default_features && ws.uses_default_features;
        if !dflt {
            tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
        }
    } else if let Some(p) = &dd.path {
        tbl.insert(
            "path",
            Value::String(Formatted::new(p.to_string_lossy().into_owned())),
        );
        if !dd.features.is_empty() {
            let mut arr = Array::new();
            for f in &dd.features {
                arr.push(Value::String(Formatted::new(f.clone())));
            }
            tbl.insert("features", Value::Array(arr));
        }
        if !dd.uses_default_features {
            tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
        }
    } else if dd.features.is_empty() && dd.uses_default_features && dd.rename.is_none() {
        return Ok(Item::Value(Value::String(Formatted::new(
            dd.version_req.clone(),
        ))));
    } else {
        tbl.insert(
            "version",
            Value::String(Formatted::new(dd.version_req.clone())),
        );
        if !dd.features.is_empty() {
            let mut arr = Array::new();
            for f in &dd.features {
                arr.push(Value::String(Formatted::new(f.clone())));
            }
            tbl.insert("features", Value::Array(arr));
        }
        if !dd.uses_default_features {
            tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
        }
    }
    if dd.rename.is_some() {
        tbl.insert("package", Value::String(Formatted::new(dd.name.clone())));
    }
    Ok(Item::Value(Value::InlineTable(tbl)))
}

fn build_tests_rs(plan: &Plan) -> String {
    let mut used_mods: BTreeSet<String> = BTreeSet::new();
    let mut out = String::new();
    for member in &plan.members {
        let prefix = sanitize_ident(&member.package_name);
        for file in &member.test_files {
            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("test");
            let stem_ident = sanitize_ident(stem);
            let mut mod_name = format!("{prefix}_{stem_ident}");
            let mut dedup = 1u32;
            while !used_mods.insert(mod_name.clone()) {
                mod_name = format!("{prefix}_{stem_ident}_{dedup}");
                dedup += 1;
            }
            out.push_str(&format!(
                "#[path = {:?}]\nmod {mod_name};\n",
                file.to_string_lossy()
            ));
        }
    }
    out
}

fn build_build_rs(plan: &Plan) -> String {
    let bin_members: Vec<&MemberPlan> = plan
        .members
        .iter()
        .filter(|m| !m.bin_names.is_empty())
        .collect();
    if bin_members.is_empty() {
        return "fn main() {}\n".to_owned();
    }
    // Standalone-aggregator build script. The aggregator has an empty
    // `[workspace]` stanza (to insulate feature unification from the
    // enclosing rudzio workspace), so `rudzio::build::expose_bins` can't
    // find bin-only workspace members via `cargo metadata` run from the
    // aggregator's manifest dir. Instead, shell out to
    // `cargo build --bins --manifest-path <bin member's Cargo.toml>` into
    // a sandboxed target dir under `OUT_DIR`, then emit
    // `cargo:rustc-env=CARGO_BIN_EXE_<bin>=<abs path>` for each bin so
    // `env!(CARGO_BIN_EXE_<name>)` in the `#[path]`-included integration
    // sources resolves at compile time.
    let workspace_root = plan.workspace_root.as_str();
    let mut out = String::new();
    out.push_str(&format!(
        "const WORKSPACE_ROOT: &str = {workspace_root:?};\n\n"
    ));
    out.push_str(BUILD_RS_HELPERS);
    out.push_str("\nfn main() -> Result<(), String> {\n");
    for member in bin_members {
        out.push_str(&format!(
            "    expose_member_bins({:?}, {:?}, &[",
            member.package_name,
            member.manifest_dir.to_string_lossy(),
        ));
        for (i, bin) in member.bin_names.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("{bin:?}"));
        }
        out.push_str("])?;\n");
    }
    out.push_str("    Ok(())\n}\n");
    out
}

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

fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    fs::canonicalize(a)
        .ok()
        .zip(fs::canonicalize(b).ok())
        .is_some_and(|(x, y)| x == y)
        || a == b
}
