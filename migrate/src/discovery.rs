//! Discovery: given the repo root, enumerate every Rust source file
//! that belongs to a Cargo package in this workspace and could plausibly
//! contain tests.
//!
//! Uses `cargo_metadata` to resolve workspace structure (so we pick up
//! per-package `Cargo.toml` locations, member paths, etc.) and the
//! `ignore` crate to walk `src/` and `tests/` while respecting
//! `.gitignore` / `.ignore` rules.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use ignore::WalkBuilder;

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub manifest_path: PathBuf,
    pub root: PathBuf,
    pub src_files: Vec<PathBuf>,
    pub tests_files: Vec<PathBuf>,
    /// Declaration-only `mod X;` items at the crate root of
    /// `src/lib.rs`, captured so the tests/main.rs scaffold can
    /// emit matching `#[path]` includes and the lib's
    /// `#[cfg(test)]`-gated suite blocks reach the integration
    /// test binary's compilation.
    pub lib_modules: Vec<LibModuleDecl>,
}

/// A `mod X;` declaration captured from `src/lib.rs`. Declaration
/// only — inline `mod X { ... }` bodies are skipped because a
/// `#[path]` include can't target a fragment inside another file.
#[derive(Debug, Clone)]
pub struct LibModuleDecl {
    /// Identifier as written in `lib.rs` (e.g. `preflight`).
    pub ident: String,
    /// Resolved path to the module's source file, relative to the
    /// package root (e.g. `src/preflight.rs` or `src/output/mod.rs`).
    pub rel_path: String,
    /// Outer attributes on the `mod X;` declaration, pre-serialised
    /// to Rust source so the scaffold can re-emit them verbatim
    /// (preserves `#[cfg(...)]`, docs, etc.). Any `#[path = "..."]`
    /// is filtered out since the scaffold emits its own path attr.
    pub attrs: Vec<String>,
}

pub fn discover(repo_root: &Path) -> Result<Vec<Package>> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(repo_root)
        .no_deps()
        .exec()
        .with_context(|| format!("running cargo metadata in {}", repo_root.display()))?;

    let mut packages = Vec::new();
    for pkg in &metadata.packages {
        let manifest_path: PathBuf = pkg.manifest_path.clone().into();
        let Some(root) = manifest_path.parent().map(Path::to_path_buf) else {
            continue;
        };
        let src_files = collect_rs(&root.join("src"));
        let tests_files = collect_rs_flat(&root.join("tests"));
        let lib_modules = collect_lib_modules(&root);
        packages.push(Package {
            name: pkg.name.to_string(),
            manifest_path,
            root,
            src_files,
            tests_files,
            lib_modules,
        });
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
}

/// Parse `src/lib.rs` if it exists and collect every top-level
/// declaration-only `mod X;` item along with its file location
/// (respecting any `#[path = "..."]` attribute) and its other
/// outer attributes.
fn collect_lib_modules(pkg_root: &Path) -> Vec<LibModuleDecl> {
    let lib_rs = pkg_root.join("src/lib.rs");
    if !lib_rs.exists() {
        return Vec::new();
    }
    let Ok(source) = fs::read_to_string(&lib_rs) else {
        return Vec::new();
    };
    let Ok(tree) = syn::parse_file(&source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in &tree.items {
        let syn::Item::Mod(m) = item else { continue };
        if m.content.is_some() {
            // Inline-body module — can't be targeted by `#[path]`.
            continue;
        }
        let ident = m.ident.to_string();
        let rel_path = match extract_path_attr(&m.attrs) {
            Some(custom) => format!("src/{custom}"),
            None => {
                // Rustc's default resolution: `src/<name>.rs` before
                // `src/<name>/mod.rs`.
                let leaf = pkg_root.join(format!("src/{ident}.rs"));
                let folder = pkg_root.join(format!("src/{ident}/mod.rs"));
                if leaf.is_file() {
                    format!("src/{ident}.rs")
                } else if folder.is_file() {
                    format!("src/{ident}/mod.rs")
                } else {
                    // Can't locate the module's source on disk; skip
                    // rather than emit a broken `#[path]`.
                    continue;
                }
            }
        };
        let attrs: Vec<String> = m
            .attrs
            .iter()
            .filter(|a| !is_path_attr(a))
            .map(|a| quote::ToTokens::to_token_stream(a).to_string())
            .collect();
        out.push(LibModuleDecl {
            ident,
            rel_path,
            attrs,
        });
    }
    out
}

fn extract_path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !is_path_attr(attr) {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(expr_lit) = &nv.value {
                if let syn::Lit::Str(s) = &expr_lit.lit {
                    return Some(s.value());
                }
            }
        }
    }
    None
}

fn is_path_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("path")
}

fn collect_rs(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let walker = WalkBuilder::new(dir)
        .standard_filters(true)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .follow_links(false)
        .build();
    let mut files: Vec<PathBuf> = walker
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(ignore::DirEntry::into_path)
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    files
}

fn collect_rs_flat(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|e| e == "rs")
                && !is_backup_file(p)
        })
        .collect();
    files.sort();
    files
}

fn is_backup_file(p: &Path) -> bool {
    p.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
        s.ends_with(crate::backup::BACKUP_SUFFIX)
    })
}
