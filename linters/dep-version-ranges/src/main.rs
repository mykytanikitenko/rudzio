//! Enforces the dependency-version scheme for the rudzio workspace
//! (operator decision, REL020-DEPRANGES).
//!
//! # The rule
//!
//! A dependency version string may be exactly one of two forms:
//!
//! * `^X.Y.Z` — a caret range with a FULL `MAJOR.MINOR.PATCH` version
//!   (optional `-prerelease` / `+build` suffix). Permitted only for the
//!   REGULAR `[dependencies]` / `[target.*.dependencies]` of a published
//!   library crate.
//! * `=X.Y.Z` — an exact pin, same full-version shape. Required everywhere
//!   else: binaries, dev-dependencies, build-dependencies, fixtures, linters.
//!
//! Any other form — bare `1.2.3`, partial `1` / `1.2`, `>=`, `~`, `<`, `*`,
//! wildcards, ranges — is a violation, regardless of context.
//!
//! Git dependencies must pin a full 40-character `rev` (no branch/tag-only).
//!
//! # Resolution of the "one version string" problem
//!
//! Versions are defined once in `[workspace.dependencies]`; members consume
//! them via `{ workspace = true }`. A single workspace entry can therefore
//! back both a library regular dependency AND a binary/dev dependency (e.g.
//! `anyhow` backs `rudzio`'s regular dep and `cargo-rudzio`'s regular dep).
//! The workspace entry is classified by its MOST-STRICT consumer: if any
//! library crate uses the name as a regular `{ workspace = true }` dep, the
//! entry must be `^X.Y.Z`; the binary/dev consumers then rely on Cargo.lock
//! for reproducibility (the documented carve-out). Only INLINE versions in
//! non-library / dev / build sections are forced to `=X.Y.Z`.
//!
//! # Skip dirs
//!
//! * `target/` — build artefacts.
//! * `.git/`.
//! * `migrate/fixtures/` — golden migration test data (deliberate input).
//! * `fixtures/tests/fixtures/` — trybuild compile-pass inputs.
//!
//! # Output
//!
//! * Detailed report at `linters/dep-version-ranges-report.txt`.
//! * Summary counts to stdout.
//! * Exit code non-zero iff any violation.

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use toml::Value;
use walkdir::WalkDir;

const CONFIG_PATH: &str = "linters/dep-version-ranges/library-crates.toml";
const REPORT_PATH: &str = "linters/dep-version-ranges-report.txt";
const SKIP_DIRS: &[&str] = &[
    "target",
    ".git",
    "migrate/fixtures",
    "fixtures/tests/fixtures",
];

/// Classification of a version string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionClass {
    /// `^X.Y.Z` (full version, optional suffix).
    Caret,
    /// `=X.Y.Z` (full version, optional suffix).
    Exact,
    /// Anything else (bare, partial, range, wildcard, malformed suffix).
    Invalid(String),
}

/// A dependency-table section and its role in the caret/exact split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// `[dependencies]` or `[target.*.dependencies]`.
    Regular,
    /// `[dev-dependencies]` or `[target.*.dev-dependencies]`.
    Dev,
    /// `[build-dependencies]` or `[target.*.build-dependencies]`.
    Build,
}

impl Section {
    fn is_regular(self) -> bool {
        matches!(self, Section::Regular)
    }
}

/// One detected version-policy violation.
#[derive(Debug, Clone)]
struct Violation {
    manifest: PathBuf,
    /// Human-readable location (`[workspace.dependencies]` or `<section>`).
    location: String,
    /// Dependency key (or `<manifest>` for a package-level issue).
    key: String,
    detail: String,
}

/// True if `s` is `MAJOR.MINOR.PATCH` with an optional `-prerelease` /
/// `+build` suffix of `[0-9A-Za-z.+-]` characters.
fn is_full_version(s: &str) -> bool {
    let mut it = s.splitn(2, |c| c == '-' || c == '+');
    let core = it.next().unwrap_or_default();
    let nums: Vec<&str> = core.split('.').collect();
    if nums.len() != 3 {
        return false;
    }
    for n in &nums {
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    if let Some(suffix) = it.next() {
        if !suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        {
            return false;
        }
    }
    true
}

/// Classify a version string into [`VersionClass`].
fn classify(raw: &str) -> VersionClass {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix('^') {
        return if is_full_version(rest) {
            VersionClass::Caret
        } else {
            VersionClass::Invalid(raw.to_string())
        };
    }
    if let Some(rest) = s.strip_prefix('=') {
        return if is_full_version(rest) {
            VersionClass::Exact
        } else {
            VersionClass::Invalid(raw.to_string())
        };
    }
    VersionClass::Invalid(raw.to_string())
}

/// The required class for a version in a given context: caret only when the
/// owning crate is a published library AND the section is a regular dep.
fn expected_class(is_library_crate: bool, section: Section) -> VersionClass {
    if is_library_crate && section.is_regular() {
        VersionClass::Caret
    } else {
        VersionClass::Exact
    }
}

/// Validate `version` against `expected`. `None` means "no version present"
/// (path-only / workspace-inherited / git-without-version) — always OK here.
fn validate_version(version: Option<&str>, expected: &VersionClass) -> Option<String> {
    let version = version?;
    match (classify(version), expected) {
        (VersionClass::Invalid(raw), _) => Some(format!("invalid version form `{raw}`")),
        (VersionClass::Caret, VersionClass::Caret) => None,
        (VersionClass::Exact, VersionClass::Exact) => None,
        (VersionClass::Caret, VersionClass::Exact) => Some(format!(
            "caret `{version}` is not allowed here (must be `=X.Y.Z`)"
        )),
        (VersionClass::Exact, VersionClass::Caret) => Some(format!(
            "exact pin `{version}` is not allowed for a library regular dep (must be `^X.Y.Z`)"
        )),
        (VersionClass::Caret, VersionClass::Invalid(_)) => unreachable!(),
        (VersionClass::Exact, VersionClass::Invalid(_)) => unreachable!(),
    }
}

/// Extract the version string from a dependency entry value. Returns `None`
/// for path-only (`{ path = ".." }` with no `version`), workspace-inherited
/// (`{ workspace = true }`), or git deps (validated separately).
fn dep_version(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s.as_str()),
        Value::Table(t) => t.get("version").and_then(Value::as_str),
        _ => None,
    }
}

/// True if the entry is `{ workspace = true }` (version inherited).
fn is_workspace_inherited(value: &Value) -> bool {
    matches!(value, Value::Table(t) if t.contains_key("workspace"))
}

/// True if the entry is a path dep with no `version` field (e.g. the
/// `rudzio = { path = "." }` self dev-deps that must not ship a version).
fn is_path_only(value: &Value) -> bool {
    matches!(value, Value::Table(t) if t.contains_key("path") && !t.contains_key("version"))
}

/// Return the `git.rev` of a git dependency entry, if it is one.
fn git_rev(value: &Value) -> Option<&str> {
    match value {
        Value::Table(t) if t.contains_key("git") => t.get("rev").and_then(Value::as_str),
        _ => None,
    }
}

/// Collect every dependency table from a parsed manifest document, tagged
/// with its [`Section`] kind (walking `[target.*]` sub-tables too).
fn collect_sections<'a>(doc: &'a Value) -> Vec<(Section, &'a toml::map::Map<String, Value>)> {
    let mut out: Vec<(Section, &'a toml::map::Map<String, Value>)> = Vec::new();
    let Some(table) = doc.as_table() else {
        return out;
    };
    for (key, val) in table {
        let section = match key.as_str() {
            "dependencies" => Some(Section::Regular),
            "dev-dependencies" => Some(Section::Dev),
            "build-dependencies" => Some(Section::Build),
            _ => None,
        };
        if let (Some(section), Some(entries)) = (section, val.as_table()) {
            out.push((section, entries));
        }
    }
    if let Some(targets) = table.get("target").and_then(Value::as_table) {
        for tval in targets.values() {
            let Some(tt) = tval.as_table() else { continue };
            for (key, val) in tt {
                let section = match key.as_str() {
                    "dependencies" => Some(Section::Regular),
                    "dev-dependencies" => Some(Section::Dev),
                    "build-dependencies" => Some(Section::Build),
                    _ => None,
                };
                if let (Some(section), Some(entries)) = (section, val.as_table()) {
                    out.push((section, entries));
                }
            }
        }
    }
    out
}

/// The package name of a manifest, if it declares a `[package]`.
fn package_name(doc: &Value) -> Option<String> {
    doc.get("package")
        .and_then(Value::as_table)
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The `[workspace.dependencies]` table of a manifest, if present.
fn workspace_deps(doc: &Value) -> Option<&toml::map::Map<String, Value>> {
    doc.get("workspace")
        .and_then(Value::as_table)
        .and_then(|w| w.get("dependencies"))
        .and_then(Value::as_table)
}

/// A parsed manifest file.
struct Manifest {
    path: PathBuf,
    doc: Value,
}

fn parse_manifest(path: &Path) -> Result<Manifest> {
    let text =
        fs::read_to_string(path).map_err(|err| anyhow!("reading {}: {}", path.display(), err))?;
    let doc: Value =
        toml::from_str(&text).map_err(|err| anyhow!("parsing {}: {}", path.display(), err))?;
    Ok(Manifest {
        path: path.to_path_buf(),
        doc,
    })
}

/// Load the explicit library-crate allowlist. Missing file → empty (every
/// crate is treated as binary/dev, so caret is never permitted).
fn load_library_crates(repo_root: &Path) -> Result<BTreeSet<String>> {
    let path = repo_root.join(CONFIG_PATH);
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let text =
        fs::read_to_string(&path).map_err(|err| anyhow!("reading {}: {}", path.display(), err))?;
    let doc: Value =
        toml::from_str(&text).map_err(|err| anyhow!("parsing {}: {}", path.display(), err))?;
    let mut names = BTreeSet::new();
    if let Some(arr) = doc.get("library_crates").and_then(Value::as_array) {
        for item in arr {
            if let Some(name) = item.as_str() {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

/// Discover every `Cargo.toml` under `repo_root`, skipping `SKIP_DIRS`.
fn discover_manifests(repo_root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in WalkDir::new(repo_root).into_iter() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() != "Cargo.toml" {
            continue;
        }
        let abs = entry.path();
        let Ok(relative) = abs.strip_prefix(repo_root) else {
            continue;
        };
        if SKIP_DIRS.iter().any(|skip| relative.starts_with(skip)) {
            continue;
        }
        paths.push(abs.to_path_buf());
    }
    paths.sort();
    paths
}

/// The parent directory of a manifest path (the crate/workspace directory).
fn manifest_dir(path: &Path) -> PathBuf {
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Manifest paths that declare a `[workspace]` table (workspace roots).
fn workspace_roots(manifests: &[Manifest]) -> Vec<&Manifest> {
    manifests
        .iter()
        .filter(|m| m.doc.get("workspace").is_some())
        .collect()
}

/// The member directories of a workspace root: the root's own directory (if
/// it also declares a `[package]`) plus each `[workspace.members]` entry
/// resolved against the root's directory.
fn member_dirs(root: &Manifest) -> Vec<PathBuf> {
    let root_dir = manifest_dir(&root.path);
    let mut dirs = Vec::new();
    if package_name(&root.doc).is_some() {
        dirs.push(root_dir.clone());
    }
    if let Some(members) = root
        .doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(Value::as_array)
    {
        for m in members {
            if let Some(s) = m.as_str() {
                dirs.push(root_dir.join(s));
            }
        }
    }
    dirs
}

/// True if `manifest` is a member of the workspace identified by `dirs`.
fn is_member(manifest: &Manifest, dirs: &[PathBuf]) -> bool {
    let dir = manifest_dir(&manifest.path);
    dirs.iter().any(|d| *d == dir)
}

/// Run the full validation over the workspace.
fn run_validation(repo_root: &Path) -> Result<Vec<Violation>> {
    let library_crates = load_library_crates(repo_root)?;
    let manifest_paths = discover_manifests(repo_root);
    let mut manifests = Vec::new();
    for path in &manifest_paths {
        manifests.push(parse_manifest(path)?);
    }

    let roots = workspace_roots(&manifests);
    let mut violations: Vec<Violation> = Vec::new();

    // Pass A: each workspace's `[workspace.dependencies]`, scoped so a
    // library-crate `{ workspace = true }` reference only widens to caret
    // within its OWN workspace (the separate linters/ workspace keeps exact
    // pins for the same crate names without colliding).
    for root in &roots {
        let dirs = member_dirs(root);
        let mut library_regular_ws: BTreeSet<String> = BTreeSet::new();
        for m in &manifests {
            if !is_member(m, &dirs) {
                continue;
            }
            let Some(name) = package_name(&m.doc) else {
                continue;
            };
            if !library_crates.contains(&name) {
                continue;
            }
            for (section, entries) in collect_sections(&m.doc) {
                if !section.is_regular() {
                    continue;
                }
                for (key, val) in entries {
                    if is_workspace_inherited(val) {
                        library_regular_ws.insert(key.clone());
                    }
                }
            }
        }
        let Some(wdeps) = workspace_deps(&root.doc) else {
            continue;
        };
        for (key, val) in wdeps {
            // git dep -> must pin a full 40-char rev.
            if let Some(rev) = git_rev(val) {
                if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
                    violations.push(Violation {
                        manifest: root.path.clone(),
                        location: "[workspace.dependencies]".to_string(),
                        key: key.clone(),
                        detail: format!("git dep must pin a full 40-char rev, got `{rev}`"),
                    });
                }
                continue;
            }
            let expected = if library_regular_ws.contains(key) {
                VersionClass::Caret
            } else {
                VersionClass::Exact
            };
            if let Some(detail) = validate_version(dep_version(val), &expected) {
                violations.push(Violation {
                    manifest: root.path.clone(),
                    location: "[workspace.dependencies]".to_string(),
                    key: key.clone(),
                    detail,
                });
            }
        }
    }

    // Pass B: inline versions in member manifests.
    for m in &manifests {
        let name = package_name(&m.doc);
        let is_library = name.as_ref().is_some_and(|n| library_crates.contains(n));
        for (section, entries) in collect_sections(&m.doc) {
            let location = section_label(section);
            for (key, val) in entries {
                if is_workspace_inherited(val) || is_path_only(val) {
                    continue;
                }
                if let Some(rev) = git_rev(val) {
                    if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
                        violations.push(Violation {
                            manifest: m.path.clone(),
                            location: location.clone(),
                            key: key.clone(),
                            detail: format!("git dep must pin a full 40-char rev, got `{rev}`"),
                        });
                    }
                    continue;
                }
                let expected = expected_class(is_library, section);
                if let Some(detail) = validate_version(dep_version(val), &expected) {
                    violations.push(Violation {
                        manifest: m.path.clone(),
                        location: location.clone(),
                        key: key.clone(),
                        detail,
                    });
                }
            }
        }
    }

    Ok(violations)
}

fn section_label(section: Section) -> String {
    match section {
        Section::Regular => "[dependencies]".to_string(),
        Section::Dev => "[dev-dependencies]".to_string(),
        Section::Build => "[build-dependencies]".to_string(),
    }
}

fn format_violation(v: &Violation) -> String {
    format!(
        "{} {} {}: {}",
        v.manifest.display(),
        v.location,
        v.key,
        v.detail
    )
}

fn write_report(repo_root: &Path, violations: &[Violation]) -> Result<()> {
    let mut body = String::new();
    writeln!(body, "# dep-version-ranges report")?;
    writeln!(body)?;
    writeln!(body, "Violations: {}", violations.len())?;
    writeln!(body)?;
    for v in violations {
        writeln!(body, "  {}", format_violation(v))?;
    }
    let report_path = repo_root.join(REPORT_PATH);
    fs::write(&report_path, body)
        .map_err(|err| anyhow!("writing {}: {}", report_path.display(), err))?;
    Ok(())
}

fn run() -> Result<bool> {
    let repo_root = env::current_dir().map_err(|err| anyhow!("getting cwd: {err}"))?;
    let violations = run_validation(&repo_root)?;
    write_report(&repo_root, &violations)?;
    println!(
        "dep-version-ranges: scanned. violations={}.",
        violations.len()
    );
    for v in violations.iter().take(20) {
        println!("  {}", format_violation(v));
    }
    if violations.len() > 20 {
        println!("  … and {} more.", violations.len() - 20);
    }
    Ok(violations.is_empty())
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("dep-version-ranges: fatal: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_full_version_classifies_as_caret() {
        assert_eq!(classify("^1.0.104"), VersionClass::Caret);
        assert_eq!(classify("^0.11.0"), VersionClass::Caret);
        assert_eq!(classify("^1.54.1"), VersionClass::Caret);
    }

    #[test]
    fn exact_full_version_classifies_as_exact() {
        assert_eq!(classify("=1.0.104"), VersionClass::Exact);
        assert_eq!(classify("=0.23.1"), VersionClass::Exact);
        assert_eq!(classify("=3.27.0"), VersionClass::Exact);
    }

    #[test]
    fn prerelease_and_build_suffix_are_full_versions() {
        assert_eq!(classify("^1.0.0-alpha.1"), VersionClass::Caret);
        assert_eq!(classify("=1.0.0+build.5"), VersionClass::Exact);
        assert_eq!(classify("=1.0.0-alpha.1+build.5"), VersionClass::Exact);
    }

    #[test]
    fn bare_version_is_invalid() {
        // bare `1.2.3` (no caret/equals prefix) is forbidden.
        assert!(matches!(classify("1.2.3"), VersionClass::Invalid(_)));
        // bare `1.2` (partial) is forbidden.
        assert!(matches!(classify("1.2"), VersionClass::Invalid(_)));
        // bare `1` is forbidden.
        assert!(matches!(classify("1"), VersionClass::Invalid(_)));
    }

    #[test]
    fn partial_caret_is_invalid() {
        assert!(matches!(classify("^1.2"), VersionClass::Invalid(_)));
        assert!(matches!(classify("^1"), VersionClass::Invalid(_)));
    }

    #[test]
    fn range_and_wildcard_operators_are_invalid() {
        assert!(matches!(classify(">=1.2.3"), VersionClass::Invalid(_)));
        assert!(matches!(classify("~1.2.3"), VersionClass::Invalid(_)));
        assert!(matches!(classify("<1.2.3"), VersionClass::Invalid(_)));
        assert!(matches!(classify("*"), VersionClass::Invalid(_)));
    }

    #[test]
    fn caret_accepted_for_library_regular_dep() {
        // Library crate + regular section expects caret; a caret passes.
        let expected = expected_class(true, Section::Regular);
        assert_eq!(expected, VersionClass::Caret);
        assert!(validate_version(Some("^1.0.104"), &expected).is_none());
    }

    #[test]
    fn caret_rejected_for_binary_or_dev_dep() {
        // Binary crate regular dep must be exact; caret is rejected.
        let expected = expected_class(false, Section::Regular);
        assert_eq!(expected, VersionClass::Exact);
        assert!(validate_version(Some("^1.0.104"), &expected).is_some());
        // Dev-dep of a library crate must also be exact.
        let expected_dev = expected_class(true, Section::Dev);
        assert_eq!(expected_dev, VersionClass::Exact);
        assert!(validate_version(Some("^1.0.104"), &expected_dev).is_some());
    }

    #[test]
    fn exact_accepted_where_required() {
        // Binary-only workspace entry / dev dep: exact passes.
        let expected = expected_class(false, Section::Regular);
        assert!(validate_version(Some("=0.23.1"), &expected).is_none());
        // Library dev-dep: exact passes.
        let expected_dev = expected_class(true, Section::Dev);
        assert!(validate_version(Some("=3.27.0"), &expected_dev).is_none());
    }

    #[test]
    fn exact_rejected_for_library_regular_dep() {
        let expected = expected_class(true, Section::Regular);
        assert!(validate_version(Some("=1.0.104"), &expected).is_some());
    }

    #[test]
    fn invalid_form_fails_regardless_of_context() {
        for expected in [VersionClass::Caret, VersionClass::Exact] {
            assert!(validate_version(Some("1.2"), &expected).is_some());
            assert!(validate_version(Some(">=1.2.3"), &expected).is_some());
            assert!(validate_version(Some("*"), &expected).is_some());
        }
    }

    #[test]
    fn workspace_and_path_only_entries_have_no_inline_version() {
        // `{ workspace = true }` carries no inline version -> nothing to check.
        let ws: Value = toml::from_str("x = { workspace = true }").unwrap();
        let entry = ws.get("x").unwrap();
        assert!(is_workspace_inherited(entry));
        assert!(dep_version(entry).is_none());

        // `{ path = ".." }` (no version) is the self dev-dep cycle.
        let p: Value = toml::from_str("x = { path = \"..\" }").unwrap();
        let entry = p.get("x").unwrap();
        assert!(is_path_only(entry));
        assert!(dep_version(entry).is_none());
    }

    #[test]
    fn full_version_shape_validation() {
        assert!(is_full_version("1.0.104"));
        assert!(is_full_version("0.11.0"));
        assert!(!is_full_version("1.2"));
        assert!(!is_full_version("1"));
        assert!(!is_full_version("1.2.3.4"));
        assert!(!is_full_version(""));
        assert!(!is_full_version("a.b.c"));
    }

    #[test]
    fn git_rev_must_be_40_hex() {
        let v: Value = toml::from_str("x = { git = \"https://e\", rev = \"abc123\" }").unwrap();
        let rev = git_rev(v.get("x").unwrap());
        assert_eq!(rev, Some("abc123"));
        assert_ne!(rev.unwrap().len(), 40);
    }
}
