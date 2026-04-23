//! Cargo.toml edits via toml_edit. Preserves comments, key order,
//! and whitespace outside the regions we touch.

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use crate::cli::RuntimeChoice;

#[derive(Debug, Default)]
pub struct ManifestEdits {
    pub needs_anyhow: bool,
    pub runtimes: std::collections::BTreeSet<RuntimeChoice>,
    pub tests_integration: Vec<IntegrationTestEntry>,
    /// Names found in the workspace's `[workspace.dependencies]`.
    /// When `rudzio` / `anyhow` is in here we emit
    /// `{ workspace = true, ... }` instead of hard-coding a version.
    pub workspace_dep_names: std::collections::BTreeSet<String>,
    /// Whether ANY src/**/*.rs file in this package was rewritten —
    /// drives the `autotests = false` decision. A tests-only
    /// migration doesn't need it; the user's lib unit tests aren't
    /// affected.
    pub had_src_conversion: bool,
    /// Whether this package has a `src/lib.rs` that could host the
    /// `#[cfg(test)] #[rudzio::main] fn main() {}` entry point.
    /// When false (bin-only crates, or libs whose root isn't at
    /// the canonical path), `[lib] harness = false` isn't safe to
    /// emit — Cargo would complain about `[lib]` with no actual
    /// lib target — and we skip that edit.
    pub has_lib_rs: bool,
}

#[derive(Debug, Clone)]
pub struct IntegrationTestEntry {
    pub name: String,
    pub path: String,
}

pub fn apply(manifest_path: &Path, edits: &ManifestEdits) -> Result<bool> {
    let source = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut doc: DocumentMut = source
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let before = doc.to_string();

    if edits.had_src_conversion {
        set_autotests_false(&mut doc);
        if edits.has_lib_rs {
            // Unit tests live in the lib's own test target; libtest
            // doesn't understand `#[rudzio::test]`, so we have to
            // swap it out for a custom main. The matching
            // `#[cfg(test)] #[rudzio::main] fn main()` in src/lib.rs
            // is handled by `run.rs::ensure_lib_has_rudzio_main`.
            // Bin-only crates have no `[lib]` target, so setting
            // `[lib] harness = false` there would tell Cargo we
            // have a library that doesn't exist.
            set_lib_harness_false(&mut doc);
        }
    }
    set_rudzio_dependency(
        &mut doc,
        &edits.runtimes,
        edits.workspace_dep_names.contains("rudzio"),
    );
    if edits.needs_anyhow {
        set_anyhow_dependency(&mut doc, edits.workspace_dep_names.contains("anyhow"));
    }
    for entry in &edits.tests_integration {
        ensure_test_entry(&mut doc, entry);
    }

    let after = doc.to_string();
    if before == after {
        return Ok(false);
    }
    let _backup = crate::backup::copy_before_write(manifest_path)
        .with_context(|| format!("backing up {}", manifest_path.display()))?;
    fs::write(manifest_path, &after)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    Ok(true)
}

fn set_autotests_false(doc: &mut DocumentMut) {
    let package = doc
        .as_table_mut()
        .entry("package")
        .or_insert(Item::Table(Table::new()));
    let Some(pkg) = package.as_table_mut() else {
        return;
    };
    let _prev = pkg.insert("autotests", value(false));
}

/// Set `[lib] harness = false` so the lib's test target runs
/// through the user's own `fn main` (i.e. `#[rudzio::main]`) rather
/// than libtest. If the user already set `harness = true`
/// explicitly we leave it — maybe they want libtest alongside.
/// Otherwise we flip it (or create the `[lib]` table if missing).
fn set_lib_harness_false(doc: &mut DocumentMut) {
    let lib = doc
        .as_table_mut()
        .entry("lib")
        .or_insert(Item::Table(Table::new()));
    let Some(lib_tbl) = lib.as_table_mut() else {
        return;
    };
    // Preserve an explicit `harness = true` override (user opted
    // into libtest on purpose — e.g. running rudzio out of a
    // separate binary via tests/main.rs aggregation).
    let user_opted_in = lib_tbl
        .get("harness")
        .and_then(|v| v.as_value())
        .and_then(toml_edit::Value::as_bool)
        .is_some_and(|b| b);
    if user_opted_in {
        return;
    }
    let _prev = lib_tbl.insert("harness", value(false));
}

fn set_rudzio_dependency(
    doc: &mut DocumentMut,
    runtimes: &std::collections::BTreeSet<RuntimeChoice>,
    workspace_pins_rudzio: bool,
) {
    if dep_already_present(doc, "rudzio") {
        return;
    }
    let features = {
        let mut arr = Array::new();
        arr.push("common");
        let mut feat_set: std::collections::BTreeSet<&'static str> =
            std::collections::BTreeSet::new();
        for rt in runtimes {
            let _inserted = feat_set.insert(rt.cargo_feature());
        }
        if feat_set.is_empty() {
            let _inserted = feat_set.insert(RuntimeChoice::TokioMt.cargo_feature());
        }
        for f in feat_set {
            arr.push(f);
        }
        arr
    };

    let mut table = InlineTable::new();
    if workspace_pins_rudzio {
        let _w = table.insert("workspace", true.into());
    } else {
        let _v = table.insert("version", "0.1".into());
    }
    let _f = table.insert("features", toml_edit::Value::from(features));

    // Library crates use rudzio at test-time only; the right home
    // is `[dev-dependencies]`. Falls back to `[dependencies]` only
    // for crates that don't have a [dev-dependencies] section
    // already (very rare — most do once any test fixture or
    // tempfile is involved).
    let dev_deps = doc
        .as_table_mut()
        .entry("dev-dependencies")
        .or_insert(Item::Table(Table::new()));
    let Some(dev_tbl) = dev_deps.as_table_mut() else {
        return;
    };
    let _prev = dev_tbl.insert("rudzio", Item::Value(table.into()));
}

fn set_anyhow_dependency(doc: &mut DocumentMut, workspace_pins_anyhow: bool) {
    if dep_already_present(doc, "anyhow") {
        return;
    }
    let entry = if workspace_pins_anyhow {
        let mut tbl = InlineTable::new();
        let _w = tbl.insert("workspace", true.into());
        Item::Value(tbl.into())
    } else {
        value("1.0")
    };
    let dev_deps = doc
        .as_table_mut()
        .entry("dev-dependencies")
        .or_insert(Item::Table(Table::new()));
    let Some(dev_tbl) = dev_deps.as_table_mut() else {
        return;
    };
    let _prev = dev_tbl.insert("anyhow", entry);
}

/// True if either `[dependencies]` or `[dev-dependencies]` already
/// declares the named crate. Used to keep the tool from clobbering
/// a manually-curated entry — features may be tuned, paths may
/// point at a workspace fork, etc.
fn dep_already_present(doc: &DocumentMut, name: &str) -> bool {
    for section in ["dependencies", "dev-dependencies"] {
        if let Some(tbl) = doc.as_table().get(section).and_then(|i| i.as_table()) {
            if tbl.contains_key(name) {
                return true;
            }
        }
    }
    false
}

fn ensure_test_entry(doc: &mut DocumentMut, entry: &IntegrationTestEntry) {
    let tests_item = doc
        .as_table_mut()
        .entry("test")
        .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let Some(arr) = tests_item.as_array_of_tables_mut() else {
        return;
    };
    // Match against either the `name` (most common — our synthesized
    // entries use the file stem as the name) or the `path` (covers
    // crates that already have a custom `[[test]] path = "..."`
    // layout pointing at the same file, possibly with a different
    // `name`). If we find a match, ensure `harness = false` is set
    // on it — otherwise rudzio's runner can't drive it — and leave
    // the other fields untouched.
    for existing in arr.iter_mut() {
        let name_match = existing
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == entry.name);
        let path_match = existing
            .get("path")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == entry.path);
        if name_match || path_match {
            let harness_is_false = existing
                .get("harness")
                .and_then(|v| v.as_bool())
                .is_some_and(|b| !b);
            if !harness_is_false {
                let _prev = existing.insert("harness", value(false));
            }
            return;
        }
    }
    let mut tbl = Table::new();
    let _prev_name = tbl.insert("name", value(entry.name.clone()));
    let _prev_path = tbl.insert("path", value(entry.path.clone()));
    let _prev_harness = tbl.insert("harness", value(false));
    arr.push(tbl);
}
