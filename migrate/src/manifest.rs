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

    set_autotests_false(&mut doc);
    set_rudzio_dependency(&mut doc, &edits.runtimes);
    if edits.needs_anyhow {
        set_anyhow_dependency(&mut doc);
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

fn set_rudzio_dependency(
    doc: &mut DocumentMut,
    runtimes: &std::collections::BTreeSet<RuntimeChoice>,
) {
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

    let deps = doc
        .as_table_mut()
        .entry("dependencies")
        .or_insert(Item::Table(Table::new()));
    let Some(deps_tbl) = deps.as_table_mut() else {
        return;
    };

    let mut table = InlineTable::new();
    let _prev_ver = table.insert("version", "0.1".into());
    let _prev_feat = table.insert("features", toml_edit::Value::from(features));
    let _prev = deps_tbl.insert("rudzio", Item::Value(table.into()));
}

fn set_anyhow_dependency(doc: &mut DocumentMut) {
    let deps = doc
        .as_table_mut()
        .entry("dependencies")
        .or_insert(Item::Table(Table::new()));
    let Some(deps_tbl) = deps.as_table_mut() else {
        return;
    };
    if !deps_tbl.contains_key("anyhow") {
        let _prev = deps_tbl.insert("anyhow", value("1.0"));
    }
}

fn ensure_test_entry(doc: &mut DocumentMut, entry: &IntegrationTestEntry) {
    let tests_item = doc
        .as_table_mut()
        .entry("test")
        .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let Some(arr) = tests_item.as_array_of_tables_mut() else {
        return;
    };
    for existing in arr.iter() {
        if existing
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == entry.name)
        {
            return;
        }
    }
    let mut tbl = Table::new();
    let _prev_name = tbl.insert("name", value(entry.name.clone()));
    let _prev_path = tbl.insert("path", value(entry.path.clone()));
    let _prev_harness = tbl.insert("harness", value(false));
    arr.push(tbl);
}
