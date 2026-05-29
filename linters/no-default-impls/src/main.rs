//! Detects every `impl Default for T` and `#[derive(Default)]`
//! attribute in the workspace. Fails CI on any hit per PLAN.md P-1
//! (Default impls removed workspace-wide; future Default impls
//! must be refactored to an explicit `empty()`/`new(...)` shape).
//!
//! # Why
//!
//! `Default` hides choices: a `T::default()` call has no source
//! location identifying which variant / which fields the caller
//! picked. Per PLAN.md P-1, workspace-internal types use explicit
//! `empty()` / `new(...)` / named-variant constructors so the
//! choice is visible at the call site. This linter guards against
//! `Default` creeping back in.
//!
//! # Scope
//!
//! Workspace-wide. Walks every `.rs` file under the repo root via
//! `walkdir`, parses each with `syn::parse_file`, and visits both
//! `ItemImpl` (for `impl Default for T`) and `Attribute` (for
//! `#[derive(Default)]`) nodes.
//!
//! # Skip dirs (built-in)
//!
//! - `target/` — cargo build artefacts.
//! - `migrate/fixtures/` — golden fixtures (deliberate input).
//! - `fixtures/tests/fixtures/` — trybuild compile-pass fixtures.
//! - `linters/expect-allow-ban/tests/fixtures/` — self-test inputs.
//! - `linters/no-default-impls/tests/fixtures/` — self-test inputs.
//!
//! # Output
//!
//! Per-hit lines to stdout. Exit code non-zero iff any hit found.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use syn::visit::Visit;
use syn::{Attribute, ItemImpl, spanned::Spanned as _};
use walkdir::WalkDir;

const SKIP_DIRS: &[&str] = &[
    "target",
    "migrate/fixtures",
    "fixtures/tests/fixtures",
    "linters/expect-allow-ban/tests/fixtures",
    "linters/no-default-impls/tests/fixtures",
];

#[derive(Debug)]
struct Hit {
    path: PathBuf,
    line: usize,
    kind: HitKind,
}

#[derive(Debug)]
enum HitKind {
    /// `impl Default for T { ... }`.
    ManualImpl(String),
    /// `#[derive(Default, ...)]` on a struct/enum.
    Derive,
}

struct Visitor<'a> {
    path: &'a Path,
    hits: Vec<Hit>,
}

impl<'a> Visitor<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, hits: Vec::new() }
    }
}

impl<'ast> Visit<'ast> for Visitor<'_> {
    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if let Some((_, trait_path, _)) = &item.trait_ {
            // Match the trait's last segment so both `Default` and
            // `core::default::Default` (etc.) get flagged.
            let last_segment = trait_path
                .segments
                .last()
                .map(|seg| seg.ident.to_string())
                .unwrap_or_default();
            if last_segment == "Default" {
                let type_name = render_self_type(&item.self_ty);
                self.hits.push(Hit {
                    path: self.path.to_path_buf(),
                    line: item.impl_token.span.start().line,
                    kind: HitKind::ManualImpl(type_name),
                });
            }
        }
        syn::visit::visit_item_impl(self, item);
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if !attr.path().is_ident("derive") {
            syn::visit::visit_attribute(self, attr);
            return;
        }
        // Iterate the comma-separated list of trait names inside
        // `derive(...)`. Each token's identity is the trait's last
        // segment for the same reason `visit_item_impl` does.
        let _ = attr.parse_nested_meta(|nested| {
            let last_segment = nested
                .path
                .segments
                .last()
                .map(|seg| seg.ident.to_string())
                .unwrap_or_default();
            if last_segment == "Default" {
                self.hits.push(Hit {
                    path: self.path.to_path_buf(),
                    line: attr.span().start().line,
                    kind: HitKind::Derive,
                });
            }
            Ok(())
        });
    }
}

fn render_self_type(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(tp) => tp
            .path
            .segments
            .iter()
            .map(|seg| seg.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "<complex type>".to_owned(),
    }
}

fn is_skipped(relative: &Path) -> bool {
    SKIP_DIRS.iter().any(|skip| relative.starts_with(skip))
}

fn collect_hits(repo_root: &Path) -> anyhow::Result<Vec<Hit>> {
    let mut hits: Vec<Hit> = Vec::new();
    for entry in WalkDir::new(repo_root).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("no-default-impls: walkdir error: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension() else {
            continue;
        };
        if ext != "rs" {
            continue;
        }
        let relative = path.strip_prefix(repo_root).unwrap_or(path);
        if is_skipped(relative) {
            continue;
        }
        let source = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("no-default-impls: reading {}: {err}", path.display());
                continue;
            }
        };
        let parsed = match syn::parse_file(&source) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("no-default-impls: parsing {}: {err}", path.display());
                continue;
            }
        };
        let mut visitor = Visitor::new(relative);
        visitor.visit_file(&parsed);
        hits.extend(visitor.hits);
    }
    Ok(hits)
}

fn main() -> ExitCode {
    let repo_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let hits = match collect_hits(&repo_root) {
        Ok(hits) => hits,
        Err(err) => {
            eprintln!("no-default-impls: scan failed: {err}");
            return ExitCode::from(2);
        }
    };
    if hits.is_empty() {
        println!("no-default-impls: scanned. total hits=0.");
        return ExitCode::SUCCESS;
    }
    println!("no-default-impls: {} hit(s) — workspace forbids Default impls per PLAN.md P-1.", hits.len());
    for hit in &hits {
        match &hit.kind {
            HitKind::ManualImpl(ty) => {
                println!(
                    "  {}:{} manual `impl Default for {}` — remove and replace with explicit `empty()` / `new(...)` constructor.",
                    hit.path.display(),
                    hit.line,
                    ty,
                );
            }
            HitKind::Derive => {
                println!(
                    "  {}:{} `#[derive(Default)]` — drop the derive and add an explicit `empty()` / `new(...)` constructor; if it's an enum, callers pick a variant explicitly.",
                    hit.path.display(),
                    hit.line,
                );
            }
        }
    }
    ExitCode::FAILURE
}
