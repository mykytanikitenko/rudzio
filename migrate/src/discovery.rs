//! Discovery: given the repo root, enumerate every Rust source file
//! that belongs to a Cargo package in this workspace and could plausibly
//! contain tests.
//!
//! Uses `cargo_metadata` to resolve workspace structure (so we pick up
//! per-package `Cargo.toml` locations, member paths, etc.) and the
//! `ignore` crate to walk `src/` and `tests/` while respecting
//! `.gitignore` / `.ignore` rules.

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
        packages.push(Package {
            name: pkg.name.to_string(),
            manifest_path,
            root,
            src_files,
            tests_files,
        });
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
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
    let Ok(rd) = std::fs::read_dir(dir) else {
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
