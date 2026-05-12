//! Discovery: given the repo root, enumerate every Rust source file
//! that belongs to a Cargo package in this workspace and could
//! plausibly contain tests.
//!
//! Uses `cargo_metadata` to resolve workspace structure (so we pick up
//! per-package `Cargo.toml` locations, member paths, etc.) and the
//! `ignore` crate to walk `src/` and `tests/` while respecting
//! `.gitignore` / `.ignore` rules.

use std::fs;
use std::path::{Path, PathBuf};
use std::result::Result as StdResult;

use anyhow::{Context as _, Result};
use ignore::WalkBuilder;

use crate::backup;

/// Per-package metadata captured by [`discover`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Package {
    /// Names of every `[[bin]]` target in the package (explicit and
    /// auto-discovered under `src/bin/`). Post-migration, each gets a
    /// `[[bin]] test = false` entry so `cargo test` doesn't fire up
    /// the default libtest harness per bin — the rudzio-main binaries
    /// have no `#[test]` fns and would otherwise emit empty
    /// "running 0 tests" noise on every run.
    pub bin_names: Vec<String>,
    /// Declaration-only `mod X;` items at the crate root of
    /// `src/lib.rs`, captured so the tests/main.rs scaffold can
    /// emit matching `#[path]` includes and the lib's
    /// `#[cfg(test)]`-gated suite blocks reach the integration
    /// test binary's compilation.
    pub lib_modules: Vec<LibModuleDecl>,
    /// Absolute path to this package's `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// Cargo package name (matches `[package].name`).
    pub name: String,
    /// Package root directory (parent of `manifest_path`).
    pub root: PathBuf,
    /// Discovered Rust source files under `src/`.
    pub src_files: Vec<PathBuf>,
    /// Discovered Rust source files under `tests/`.
    pub tests_files: Vec<PathBuf>,
    /// True when the lib has anything beyond a pure organizer shape
    /// — items at crate root, `mod.rs`-form submodules, or
    /// submodule files with their own nested `mod X;` declarations.
    /// The scaffold then emits `#[path = "../src/lib.rs"] mod __lib;
    /// pub use __lib::*;` instead of per-submodule `#[path]`
    /// includes; per-file includes can't reach root-level items
    /// and Rust's nested submodule resolution doesn't honour the
    /// parent's `#[path]`.
    pub uses_lib_aggregation: bool,
}

/// A `mod X;` declaration captured from `src/lib.rs`.
///
/// Declaration only — inline `mod X { ... }` bodies are skipped
/// because a `#[path]` include can't target a fragment inside another
/// file.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LibModuleDecl {
    /// Outer attributes on the `mod X;` declaration, pre-serialised
    /// to Rust source so the scaffold can re-emit them verbatim
    /// (preserves `#[cfg(...)]`, docs, etc.). Any `#[path = "..."]`
    /// is filtered out since the scaffold emits its own path attr.
    pub attrs: Vec<String>,
    /// Identifier as written in `lib.rs` (e.g. `preflight`).
    pub ident: String,
    /// Resolved path to the module's source file, relative to the
    /// package root (e.g. `src/preflight.rs` or `src/output/mod.rs`).
    pub rel_path: String,
}

/// Enumerate every Cargo package rooted under `repo_root` and the
/// Rust files (under `src/` and `tests/`) plausibly part of each.
///
/// # Errors
///
/// Returns the underlying `cargo metadata` error if metadata
/// collection fails (no manifest, broken workspace, etc.).
#[inline]
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
        // tests/ is walked recursively — crates with custom
        // `[[test]] path = "tests/<suite>/mod.rs"` layouts keep
        // source files in deeper subdirs like
        // tests/integration/db/repository/files/create.rs, and a
        // non-recursive scan would miss them and silently no-op.
        let tests_files = collect_rs(&root.join("tests"));
        let lib_modules = collect_lib_modules(&root);
        let uses_lib_aggregation = needs_lib_aggregation(&root, &lib_modules);
        let mut bin_names: Vec<String> = pkg
            .targets
            .iter()
            .filter(|target| {
                target
                    .kind
                    .iter()
                    .any(|kind| matches!(kind, cargo_metadata::TargetKind::Bin))
            })
            .map(|target| target.name.clone())
            .collect();
        bin_names.sort();
        bin_names.dedup();
        packages.push(Package {
            bin_names,
            lib_modules,
            manifest_path,
            name: pkg.name.to_string(),
            root,
            src_files,
            tests_files,
            uses_lib_aggregation,
        });
    }
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(packages)
}

/// Walk `dir` recursively (respecting `.gitignore` / `.ignore`) and
/// return every `.rs` file path, sorted, excluding `.backup_*` files
/// emitted by a prior run of this tool.
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
        .filter_map(StdResult::ok)
        .filter(|entry| entry.path().is_file())
        .map(ignore::DirEntry::into_path)
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs") && !is_backup_file(path))
        .collect();
    files.sort();
    files
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
        let syn::Item::Mod(decl) = item else { continue };
        if decl.content.is_some() {
            // Inline-body module — can't be targeted by `#[path]`.
            continue;
        }
        let ident = decl.ident.to_string();
        let rel_path = if let Some(custom) = extract_path_attr(&decl.attrs) {
            format!("src/{custom}")
        } else {
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
        };
        let attrs: Vec<String> = decl
            .attrs
            .iter()
            .filter(|attr| !is_path_attr(attr))
            .map(|attr| quote::ToTokens::to_token_stream(attr).to_string())
            .collect();
        out.push(LibModuleDecl {
            attrs,
            ident,
            rel_path,
        });
    }
    out
}

/// Pull the value of `#[path = "..."]` from a list of attributes, if
/// any.
fn extract_path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !is_path_attr(attr) {
            continue;
        }
        if let syn::Meta::NameValue(name_value) = &attr.meta
            && let syn::Expr::Lit(expr_lit) = &name_value.value
            && let syn::Lit::Str(literal) = &expr_lit.lit
        {
            return Some(literal.value());
        }
    }
    None
}

/// True if `path` looks like a file this tool emitted as a backup.
fn is_backup_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(backup::SUFFIX))
}

/// True if `attr` is a `#[path = "..."]` attribute.
fn is_path_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("path")
}

/// True if the lib should be aggregated as a whole (`mod __lib;`)
/// instead of via per-submodule `#[path]` includes. Triggers on:
/// - lib.rs has any item that isn't `mod` / `use` / inner attrs
///   (functions, structs, traits, consts, impls, type aliases…
///   anything that lives at the lib's crate root and would be
///   invisible to per-submodule path includes);
/// - any top-level `mod X;` resolves to `src/X/mod.rs` (directory
///   form — submodules have a tree underneath them and Rust's
///   nested module resolution doesn't honour the parent's
///   `#[path]` attribute);
/// - any submodule file (the resolved path of a top-level `mod X;`)
///   declares its own `mod Y;` with no inline body and no
///   explicit `#[path]` attr — same nested-resolution problem.
fn needs_lib_aggregation(pkg_root: &Path, lib_modules: &[LibModuleDecl]) -> bool {
    let lib_rs = pkg_root.join("src/lib.rs");
    if !lib_rs.is_file() {
        return false;
    }
    let Ok(source) = fs::read_to_string(&lib_rs) else {
        return false;
    };
    let Ok(tree) = syn::parse_file(&source) else {
        return false;
    };
    for item in &tree.items {
        match item {
            syn::Item::Mod(_) | syn::Item::Use(_) | syn::Item::ExternCrate(_) => {}
            // Anything with body content at lib-root scope lives
            // outside per-submodule `#[path]` reach — flip to
            // whole-lib aggregation. syn::Item is non_exhaustive, so
            // the wildcard preserves the same answer for future
            // variants.
            _other @ (syn::Item::Const(_)
            | syn::Item::Enum(_)
            | syn::Item::Fn(_)
            | syn::Item::ForeignMod(_)
            | syn::Item::Impl(_)
            | syn::Item::Macro(_)
            | syn::Item::Static(_)
            | syn::Item::Struct(_)
            | syn::Item::Trait(_)
            | syn::Item::TraitAlias(_)
            | syn::Item::Type(_)
            | syn::Item::Union(_)
            | syn::Item::Verbatim(_))
            | _other => return true,
        }
    }
    for decl in lib_modules {
        // Directory-form submodule (src/X/mod.rs) → nested.
        if decl.rel_path.ends_with("/mod.rs") {
            return true;
        }
        // Submodule file declares its own non-`#[path]` `mod Y;`.
        let file = pkg_root.join(&decl.rel_path);
        if let Ok(submodule_source) = fs::read_to_string(&file)
            && let Ok(submodule_tree) = syn::parse_file(&submodule_source)
        {
            for item in &submodule_tree.items {
                if let syn::Item::Mod(sub) = item
                    && sub.content.is_none()
                    && extract_path_attr(&sub.attrs).is_none()
                {
                    return true;
                }
            }
        }
    }
    false
}
