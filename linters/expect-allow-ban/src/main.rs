//! Detects every `#[expect(...)]` and `#[allow(...)]` attribute in
//! the workspace and fails CI on any not pre-justified in
//! `linters/expect-allow-ban/allowlist.toml`. Both outer (`#[…]`)
//! and inner (`#![…]`) attribute styles are treated identically.
//!
//! # Why
//!
//! Lint-suppression attributes carry an escape-hatch risk: an
//! `#[expect]` or `#[allow]` silently disables review-grade
//! signal. Especially with LLM-assisted code generation, the
//! `reason = "..."` field has been observed to ship with plausible
//! but unvetted justifications. This linter forces an explicit
//! per-site human-vetted disposition: either remove the
//! suppression (fix the underlying code) or pre-justify it in
//! `allowlist.toml` with a fresh reason.
//!
//! # Scope
//!
//! Workspace-wide. Walks every `.rs` file under the repo root via
//! `walkdir`, parses each with `syn::parse_file`, and visits all
//! `Attribute` nodes. Comments and macro-emitted string-literal
//! contents are invisible to the AST parser, so they are naturally
//! excluded.
//!
//! # Skip dirs (built-in)
//!
//! - `target/` — cargo build artefacts.
//! - `migrate/fixtures/` — golden fixtures (deliberate input).
//! - `fixtures/tests/fixtures/` — trybuild compile-pass fixtures
//!   (deliberate Bug #15 macro-shape input).
//! - `linters/expect-allow-ban/tests/fixtures/` — this linter's
//!   own self-test inputs.
//!
//! # Allowlist schema
//!
//! ```toml
//! [[entry]]
//! path = "src/runner.rs"        # repo-relative
//! line = 794                    # 1-based attribute line
//! kind = "expect"               # "expect" | "allow"
//! lints = ["unsafe_code"]       # exact set, order-insensitive
//! reason = "<vetted explanation, not auto-extracted>"
//! ```
//!
//! Empty or missing allowlist → every detected attribute is a
//! hazard.
//!
//! # Output
//!
//! - Detailed report at `linters/expect-allow-ban-report.txt`
//!   (per-hit lines).
//! - Summary counts to stdout.
//! - Exit code non-zero iff any unmatched hit OR orphan allowlist
//!   entry exists.
//!
//! # Known limitations (future work)
//!
//! - `#[cfg_attr(<cond>, allow(...))]` and `#[cfg_attr(<cond>,
//!   expect(...))]` are NOT detected — the outer attribute path is
//!   `cfg_attr`, not `allow`/`expect`. Conditional suppression
//!   remains an escape hatch.
//! - No de-duplication when the same source file appears under
//!   multiple workspace roots (symlinks).

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;
use syn::visit::Visit;
use syn::{Attribute, spanned::Spanned as _};
use walkdir::WalkDir;

const ALLOWLIST_PATH: &str = "linters/expect-allow-ban/allowlist.toml";
const REPORT_PATH: &str = "linters/expect-allow-ban-report.txt";
const SKIP_DIRS: &[&str] = &[
    "target",
    "migrate/fixtures",
    "fixtures/tests/fixtures",
    "linters/expect-allow-ban/tests/fixtures",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttrKind {
    Allow,
    Expect,
}

impl AttrKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Expect => "expect",
        }
    }

    fn from_attr_path(attr: &Attribute) -> Option<Self> {
        if attr.path().is_ident("expect") {
            Some(Self::Expect)
        } else if attr.path().is_ident("allow") {
            Some(Self::Allow)
        } else {
            None
        }
    }
}

/// One detected attribute occurrence.
#[derive(Debug, Clone)]
struct Hit {
    path: PathBuf,
    line: usize,
    kind: AttrKind,
    lints: Vec<String>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AllowlistEntry {
    path: String,
    line: usize,
    kind: String,
    lints: Vec<String>,
    /// Human-authored justification; rendered into the orphan
    /// report so a reviewer of a failing CI can see what each
    /// allowlisted entry was claiming about itself.
    reason: String,
}

#[derive(Debug, Deserialize, Default)]
struct Allowlist {
    #[serde(default)]
    entry: Vec<AllowlistEntry>,
}

struct AttrVisitor<'a> {
    path: &'a Path,
    hits: Vec<Hit>,
}

impl<'a> AttrVisitor<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, hits: Vec::new() }
    }
}

impl<'ast> Visit<'ast> for AttrVisitor<'_> {
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        let Some(kind) = AttrKind::from_attr_path(attr) else {
            return;
        };
        let (lints, reason) = parse_nested(attr);
        let line = attr.span().start().line;
        self.hits.push(Hit {
            path: self.path.to_path_buf(),
            line,
            kind,
            lints,
            reason,
        });
    }
}

/// Render a `syn::Path` as `seg1::seg2::seg3` (no generics, no leading `::`).
fn path_to_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|seg| seg.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// Parse an `#[allow(...)]` / `#[expect(...)]` attribute's nested
/// contents into a list of lint names and an optional `reason`
/// string. Tolerates malformed input — anything unrecognised is
/// silently skipped (the goal is to surface the existence of the
/// suppression, not to validate its internals).
fn parse_nested(attr: &Attribute) -> (Vec<String>, Option<String>) {
    let mut lints: Vec<String> = Vec::new();
    let mut reason: Option<String> = None;
    let _ = attr.parse_nested_meta(|nested| {
        if nested.path.is_ident("reason") {
            let value = nested.value()?;
            let lit: syn::LitStr = value.parse()?;
            reason = Some(lit.value());
        } else {
            lints.push(path_to_string(&nested.path));
        }
        Ok(())
    });
    (lints, reason)
}

/// True if any element of `SKIP_DIRS` is a prefix of `relative`.
fn is_skipped(relative: &Path) -> bool {
    SKIP_DIRS
        .iter()
        .any(|skip| relative.starts_with(skip))
}

/// Load the allowlist from `ALLOWLIST_PATH`. Missing file → empty
/// allowlist (every attribute becomes a hazard).
fn load_allowlist(repo_root: &Path) -> anyhow::Result<Allowlist> {
    let path = repo_root.join(ALLOWLIST_PATH);
    if !path.exists() {
        return Ok(Allowlist::default());
    }
    let text = fs::read_to_string(&path)
        .map_err(|err| anyhow::anyhow!("reading {}: {}", path.display(), err))?;
    let parsed: Allowlist = toml::from_str(&text)
        .map_err(|err| anyhow::anyhow!("parsing {}: {}", path.display(), err))?;
    Ok(parsed)
}

/// Walk the workspace from `repo_root`, parse every `.rs` file, and
/// collect every `#[expect]` / `#[allow]` attribute occurrence.
fn collect_hits(repo_root: &Path) -> anyhow::Result<Vec<Hit>> {
    let mut hits: Vec<Hit> = Vec::new();
    for entry in WalkDir::new(repo_root).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("warning: walkdir error: {err}");
                continue;
            }
        };
        let abs_path = entry.path();
        if !abs_path.is_file() {
            continue;
        }
        if abs_path.extension().and_then(|os| os.to_str()) != Some("rs") {
            continue;
        }
        let Ok(relative) = abs_path.strip_prefix(repo_root) else {
            continue;
        };
        if is_skipped(relative) {
            continue;
        }
        let text = match fs::read_to_string(abs_path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("warning: reading {}: {}", abs_path.display(), err);
                continue;
            }
        };
        let parsed = match syn::parse_file(&text) {
            Ok(file) => file,
            Err(err) => {
                eprintln!(
                    "warning: syn::parse_file failed for {}: {}",
                    abs_path.display(),
                    err
                );
                continue;
            }
        };
        let mut visitor = AttrVisitor::new(relative);
        visitor.visit_file(&parsed);
        hits.extend(visitor.hits);
    }
    Ok(hits)
}

/// True iff `hit` matches `entry`: same path, same line, same
/// kind, same set of lints (order-insensitive).
fn matches(hit: &Hit, entry: &AllowlistEntry) -> bool {
    if hit.line != entry.line {
        return false;
    }
    if hit.kind.as_str() != entry.kind {
        return false;
    }
    if hit.path != Path::new(&entry.path) {
        return false;
    }
    let hit_lints: BTreeSet<&str> = hit.lints.iter().map(String::as_str).collect();
    let entry_lints: BTreeSet<&str> = entry.lints.iter().map(String::as_str).collect();
    hit_lints == entry_lints
}

/// Compare hits against allowlist. Returns (unmatched hits, orphan entry indices).
fn diff(hits: &[Hit], allowlist: &Allowlist) -> (Vec<Hit>, Vec<usize>) {
    let mut matched_entries: BTreeSet<usize> = BTreeSet::new();
    let mut unmatched: Vec<Hit> = Vec::new();
    for hit in hits {
        let mut found = false;
        for (idx, entry) in allowlist.entry.iter().enumerate() {
            if matches(hit, entry) {
                matched_entries.insert(idx);
                found = true;
                break;
            }
        }
        if !found {
            unmatched.push(hit.clone());
        }
    }
    let orphans: Vec<usize> = (0..allowlist.entry.len())
        .filter(|idx| !matched_entries.contains(idx))
        .collect();
    (unmatched, orphans)
}

/// Format a hit as one report line.
fn format_hit(hit: &Hit) -> String {
    let lints_joined = hit.lints.join(",");
    let reason_summary = match hit.reason.as_deref() {
        Some(text) => {
            let trimmed: String = text.chars().take(80).collect();
            format!("\"{trimmed}\"")
        }
        None => "<MISSING>".to_string(),
    };
    format!(
        "{}:{} kind={} lints=[{lints_joined}] reason={reason_summary}",
        hit.path.display(),
        hit.line,
        hit.kind.as_str(),
    )
}

/// Write the detailed report file.
fn write_report(
    repo_root: &Path,
    hits: &[Hit],
    unmatched: &[Hit],
    orphans: &[usize],
    allowlist: &Allowlist,
) -> anyhow::Result<()> {
    let mut body = String::new();
    writeln!(body, "# expect-allow-ban report")?;
    writeln!(body)?;
    writeln!(body, "Total real `#[expect]` / `#[allow]` attributes found: {}", hits.len())?;
    writeln!(body, "  - allowlisted (PASS): {}", hits.len().saturating_sub(unmatched.len()))?;
    writeln!(body, "  - unallowlisted (FAIL): {}", unmatched.len())?;
    writeln!(body, "Orphan allowlist entries (no matching attr): {}", orphans.len())?;
    writeln!(body)?;
    writeln!(body, "## All hits")?;
    writeln!(body)?;
    for hit in hits {
        writeln!(body, "  {}", format_hit(hit))?;
    }
    writeln!(body)?;
    writeln!(body, "## Unallowlisted hits (FAIL)")?;
    writeln!(body)?;
    for hit in unmatched {
        writeln!(body, "  {}", format_hit(hit))?;
    }
    writeln!(body)?;
    writeln!(body, "## Orphan allowlist entries (FAIL)")?;
    writeln!(body)?;
    for &idx in orphans {
        if let Some(entry) = allowlist.entry.get(idx) {
            writeln!(
                body,
                "  {}:{} kind={} lints={:?} reason={:?}",
                entry.path, entry.line, entry.kind, entry.lints, entry.reason
            )?;
        }
    }
    let report_path = repo_root.join(REPORT_PATH);
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| anyhow::anyhow!("creating {}: {}", parent.display(), err))?;
    }
    fs::write(&report_path, body)
        .map_err(|err| anyhow::anyhow!("writing {}: {}", report_path.display(), err))?;
    Ok(())
}

fn print_summary(hits: &[Hit], unmatched: &[Hit], orphans: &[usize]) {
    println!(
        "expect-allow-ban: scanned. total hits={}, unallowlisted={}, orphans={}.",
        hits.len(),
        unmatched.len(),
        orphans.len(),
    );
    if !unmatched.is_empty() {
        println!(
            "expect-allow-ban: {} unallowlisted attribute(s) — see {} for the full list.",
            unmatched.len(),
            REPORT_PATH,
        );
        for hit in unmatched.iter().take(10) {
            println!("  {}", format_hit(hit));
        }
        if unmatched.len() > 10 {
            println!("  … and {} more.", unmatched.len() - 10);
        }
    }
    if !orphans.is_empty() {
        println!(
            "expect-allow-ban: {} orphan allowlist entry/entries (no matching attr).",
            orphans.len(),
        );
    }
}

fn run() -> anyhow::Result<bool> {
    let repo_root = env::current_dir()
        .map_err(|err| anyhow::anyhow!("getting current dir: {err}"))?;
    let allowlist = load_allowlist(&repo_root)?;
    let hits = collect_hits(&repo_root)?;
    let (unmatched, orphans) = diff(&hits, &allowlist);
    write_report(&repo_root, &hits, &unmatched, &orphans, &allowlist)?;
    print_summary(&hits, &unmatched, &orphans);
    Ok(unmatched.is_empty() && orphans.is_empty())
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("expect-allow-ban: fatal: {err:#}");
            ExitCode::FAILURE
        }
    }
}
