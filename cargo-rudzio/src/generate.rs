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

/// Top-level member entries that must NOT be symlinked into the bridge
/// dir: either they'd collide with bridge-synthesised files
/// (`Cargo.toml`, `build.rs`), they're noise cargo regenerates anyway
/// (`Cargo.lock`), or they'd be actively harmful (`target/` creates
/// parallel-build recursion, `.git/` confuses tooling that walks up).
const BRIDGE_SKIPLIST: &[&str] = &["Cargo.toml", "Cargo.lock", "build.rs", "target", ".git"];

/// Everything extracted from `cargo metadata` plus the workspace root's
/// Cargo.toml that the generator needs to emit the aggregator.
#[derive(Debug)]
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

#[derive(Clone, Debug)]
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
    /// Rust edition declared in the member's `[package] edition` — the
    /// bridge crate uses this so its generated manifest matches the
    /// compilation semantics of the real source tree it re-points at.
    pub edition: String,
    /// Absolute path to the member's `src/lib.rs` (when `has_lib`). The
    /// bridge crate's `[lib] path` points here so cargo compiles the
    /// real source tree instead of the bridge dir.
    pub src_lib_path: Option<PathBuf>,
    /// `true` iff the member has at least one file under `src/**` that
    /// syntactically declares a rudzio suite or gates a module on the
    /// `rudzio_test` cfg. Drives bridge-crate generation: the bridge
    /// exists specifically to make `[dev-dependencies]` visible to the
    /// member's src tree under `--cfg rudzio_test`, so members whose
    /// src has no rudzio surface don't need one.
    pub has_src_rudzio_suite: bool,
    /// The member's own `[features]` table, mirrored verbatim into the
    /// bridge's `[features]` so `cfg(feature = "...")` gates in member
    /// source resolve against the same universe of feature names they
    /// would under the member's own `cargo test`.
    pub features: BTreeMap<String, Vec<String>>,
    /// Features listed in `[package.metadata.rudzio] features = [...]`
    /// — the member's explicit opt-in for features that should be
    /// active under `cargo rudzio test` but aren't in `default`. These
    /// get unioned with the member's `default` to form the bridge's
    /// own `default` feature list.
    pub rudzio_activated_features: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitRef {
    Rev(String),
    Branch(String),
    Tag(String),
}

#[derive(Clone, Debug)]
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
    /// `optional = true` in the member's `[dependencies]` entry.
    /// Mirrored into the bridge so `dep:X` references in the bridge's
    /// `[features]` table resolve (cargo requires the dep to be
    /// optional for `dep:` syntax to be valid).
    pub optional: bool,
}

#[derive(Clone, Debug)]
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

        let src_lib_path = pkg
            .targets
            .iter()
            .find(|t| t.kind.iter().any(|k| matches!(k, TargetKind::Lib)))
            .map(|t| t.src_path.as_std_path().to_path_buf());

        let edition = pkg.edition.to_string();

        let dev_deps =
            read_dev_deps(pkg.manifest_path.as_std_path()).with_context(|| {
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
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let rudzio_activated_features =
            load_rudzio_activated_features(pkg.manifest_path.as_std_path())
                .with_context(|| {
                    format!(
                        "loading `[package.metadata.rudzio].features` from {}",
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
            edition,
            src_lib_path,
            has_src_rudzio_suite,
            features,
            rudzio_activated_features,
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

/// Substring scan of every `*.rs` file under `src_root` for the markers
/// that indicate a rudzio suite or `--cfg rudzio_test`-gated module lives
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
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
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

/// Scan `member`'s `src/**/*.rs` for `#[cfg(test)]`-gated modules that
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
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
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
            let tail: Vec<&str> = text
                .lines()
                .skip(line_idx + 1)
                .take(6)
                .filter(|l| !l.trim_start().is_empty())
                .collect();
            let next_non_attr = tail.iter().find(|l| !l.trim_start().starts_with("#["));
            let gates_a_mod = next_non_attr
                .is_some_and(|l| l.trim_start().starts_with("mod ") || l.trim_start().starts_with("pub mod "));
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
                line = line_idx + 1,
            ));
        }
    }
    out
}

/// Plan-level convenience: scan every member's src tree and return the
/// union of warnings in member order (matching `Plan.members` which
/// `build_plan` sorts by package name, so output is deterministic).
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
pub fn collect_rudzio_spec(
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
pub fn build_rudzio_inline_table(spec: &RudzioSpec) -> InlineTable {
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

/// Read the `[package.metadata.rudzio] features = [...]` array from a
/// member manifest. These names are features the member opts in to
/// under `cargo rudzio test` — they become part of the bridge's own
/// `default` feature list so cargo activates them when compiling the
/// bridge. Missing → empty vec (no opt-in). Non-array value → hard
/// error so misconfigurations surface at aggregator-generation time.
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
        let s = entry.as_str().ok_or_else(|| {
            anyhow!(
                "`[package.metadata.rudzio].features` in {} must contain only strings",
                manifest_path.display()
            )
        })?;
        out.push(s.to_owned());
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
        for (_cfg, cfg_item) in target_tbl.iter() {
            let Some(cfg_tbl) = cfg_item.as_table() else {
                continue;
            };
            for section in sections {
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
        optional: false,
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
        "optional" => {
            if let Some(b) = val.as_bool() {
                spec.optional = b;
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
            fs::remove_file(&path)
                .with_context(|| format!("removing {}", path.display()))?;
        }
    }
    if src_dir.exists() {
        fs::remove_dir_all(&src_dir)
            .with_context(|| format!("removing {}", src_dir.display()))?;
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

/// The two gates a member must pass to get a bridge crate generated:
/// it has to be a lib (bridges re-point `[lib] path`, nothing to
/// re-point for a bin-only crate) AND it has to have rudzio surface
/// inside `src/**` (otherwise nothing in the member's own compilation
/// unit needs dev-deps — integration tests in `tests/*.rs` are already
/// pulled into the aggregator's compilation unit instead and see the
/// aggregator's deps directly).
pub fn bridge_applies_to(member: &MemberPlan) -> bool {
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
pub fn write_bridge_crate(plan: &Plan, member: &MemberPlan, out: &Path) -> Result<()> {
    let bridge_dir = out.join(bridge_dir_name(member));
    // Wipe any pre-existing bridge directory so stale symlinks from a
    // prior run (member entry renamed or removed) don't survive.
    if bridge_dir.exists() {
        fs::remove_dir_all(&bridge_dir).with_context(|| {
            format!("wiping stale bridge dir {}", bridge_dir.display())
        })?;
    }
    fs::create_dir_all(&bridge_dir)
        .with_context(|| format!("creating bridge dir {}", bridge_dir.display()))?;
    let manifest = build_bridge_cargo_toml(plan, member)?;
    fs::write(bridge_dir.join("Cargo.toml"), manifest).with_context(|| {
        format!("writing bridge Cargo.toml at {}", bridge_dir.display())
    })?;
    fs::write(bridge_dir.join("build.rs"), build_bridge_build_rs(member)).with_context(|| {
        format!("writing bridge build.rs at {}", bridge_dir.display())
    })?;
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
/// If the member's manifest_dir is unreadable (e.g. synthetic test
/// fixtures using a fake path), the symlink pass is a no-op. Bridge
/// compile would fail anyway in that case — the no-op keeps synthetic
/// unit tests of Cargo.toml emission unaffected.
fn symlink_member_tree_into_bridge(member_dir: &Path, bridge_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(member_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if BRIDGE_SKIPLIST.iter().any(|s| *s == name_str) {
            continue;
        }
        let src = entry.path();
        let dst = bridge_dir.join(&name);
        create_symlink(&src, &dst).with_context(|| {
            format!(
                "symlinking bridge entry {} -> {}",
                dst.display(),
                src.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_symlink(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cargo-rudzio bridge symlinking is not yet implemented on Windows",
    ))
}

/// Synthesize the bridge's `build.rs`. Every bridge gets one — it is
/// the per-compile-unit emission site for `cargo:rustc-cfg=rudzio_test`
/// that scopes the cfg to the bridge's rustc invocation only (as
/// opposed to propagating it through ambient `RUSTFLAGS`, which leaked
/// into nested `cargo build --bins` and caused thousands of unresolved
/// -crate errors in downstream bin crates).
///
/// For bin-bearing members the build.rs additionally invokes
/// `expose_member_bins` so `cargo:rustc-env=CARGO_BIN_EXE_<bin>=<abs>`
/// reaches the bridge's compile unit, where `rudzio::bin!(...)`
/// ultimately expands. The member's own `build.rs` is never forwarded:
/// its emissions would scope to the member's standalone compile unit,
/// but the cfg needs to land on the bridge.
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
    out.push_str("])?;\n    Ok(())\n}\n");
    out
}

pub fn bridge_package_name(member: &MemberPlan) -> String {
    format!("{}_rudzio_bridge", crate_name_to_ident(&member.package_name))
}

pub fn bridge_dir_name(member: &MemberPlan) -> String {
    crate_name_to_ident(&member.package_name)
}

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
        .strip_prefix(&member.manifest_dir)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| PathBuf::from("src").join("lib.rs"));
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
        for v in values {
            arr.push(v.as_str());
        }
        tbl.insert(key, value(arr));
    }

    let mut default: BTreeSet<String> = BTreeSet::new();
    if let Some(member_default) = member.features.get("default") {
        for f in member_default {
            let _ = default.insert(f.clone());
        }
    }
    for f in &member.rudzio_activated_features {
        let _ = default.insert(f.clone());
    }

    let mut default_arr = Array::new();
    for f in &default {
        default_arr.push(f.as_str());
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
    for dd in &member.dev_deps {
        let entry_name = dd.rename.as_deref().unwrap_or(&dd.name).to_owned();
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

    for (entry_name, dd) in &merged {
        let item = render_dev_dep(dd, plan)?;
        deps.insert(entry_name, item);
    }

    // Bridges exist to expose dev-deps under `--cfg rudzio_test`, and
    // rudzio itself is the universally-required one. If the member
    // declared rudzio only under `[dev-dependencies]` the merge above
    // already surfaces it; if the member didn't declare rudzio at all
    // (defensive — we already filter to rudzio-using members) we inject
    // the aggregator's unified spec so `use ::rudzio::*` still resolves.
    if !deps.contains_key(RUDZIO_DEP) {
        let tbl = build_rudzio_inline_table(&plan.rudzio_spec);
        deps.insert(RUDZIO_DEP, Item::Value(Value::InlineTable(tbl)));
    }

    Ok(deps)
}

/// Build the aggregator's `src/main.rs`. Emits `extern crate <crate>;`
/// for every member we also list as a path dep in the aggregator's
/// `[dependencies]`, which forces rustc to actually link each member's
/// rlib into the final binary. Without this, rustc drops unreferenced
/// rlibs during link-time DCE and linkme's `#[link_section]` statics in
/// those crates (even annotated `#[used]`, which only blocks in-object
/// DCE) never reach the binary — meaning `#[rudzio::test]` fns under a
/// member's `src/**` wouldn't be discovered at run time. The member
/// package name is normalised hyphens→underscores to match cargo's own
/// crate-name-to-ident rule.
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
        out.push_str(&format!("extern crate {ident};\n"));
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
    } else if dd.features.is_empty()
        && dd.uses_default_features
        && dd.rename.is_none()
        && !dd.optional
    {
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
    if dd.optional {
        tbl.insert("optional", Value::Boolean(Formatted::new(true)));
    }
    Ok(Item::Value(Value::InlineTable(tbl)))
}

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
        let mut dedup = 1u32;
        while !per_crate_mod_names.insert(crate_mod.clone()) {
            crate_mod = format!("{base_ident}_{dedup}");
            dedup += 1;
        }
        out.push_str(&format!("mod {crate_mod} {{\n"));

        let mut used_inner: BTreeSet<String> = BTreeSet::new();
        for file in &member.test_files {
            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("test");
            let stem_ident = sanitize_ident(stem);
            let mut inner = stem_ident.clone();
            let mut d = 1u32;
            while !used_inner.insert(inner.clone()) {
                inner = format!("{stem_ident}_{d}");
                d += 1;
            }
            out.push_str(&format!(
                "    #[path = {:?}]\n    mod {inner};\n",
                file.to_string_lossy()
            ));
        }
        out.push_str("}\n");
    }
    out
}

fn build_build_rs(plan: &Plan) -> String {
    let bin_members: Vec<&MemberPlan> = plan
        .members
        .iter()
        .filter(|m| !m.bin_names.is_empty())
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

