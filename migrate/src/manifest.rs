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
    /// `[[bin]]` target names from `cargo metadata`. Each one gets a
    /// `[[bin]] test = false` entry in the manifest after migration,
    /// so the rudzio-main binaries don't fire libtest on every
    /// `cargo test` pass with zero test functions to report.
    pub bin_names: Vec<String>,
    /// Whether any rewritten file in this package emits a reference to
    /// the `rudzio_test` cfg symbol (via the `cfg(any(test,
    /// rudzio_test))` rewrite or the synthesized integration-file
    /// wrapper module). When true, the package's Cargo.toml needs a
    /// `[lints.rust] unexpected_cfgs = { check-cfg = ['cfg(rudzio_test)'] }`
    /// entry so Rust 1.80+'s unknown-cfg warning doesn't fire.
    pub needs_rudzio_test_cfg: bool,
    /// Rust idents referenced from inside src-embedded rudzio suites
    /// (collected by the rewriter's suite-module walker, filtered
    /// against `[dev-dependencies]` keys at apply time). When
    /// non-empty, the named dev-deps are copied into
    /// `[target."cfg(rudzio_test)".dependencies]` so the aggregator's
    /// plain-lib build (no dev-deps) can still resolve them under
    /// `--cfg rudzio_test`. Empty for tests-only migrations —
    /// `tests/*.rs` gets `#[path]`-included into the aggregator crate,
    /// which lists the deps itself, so mirroring would be pointless.
    pub mirror_crate_idents: std::collections::BTreeSet<String>,
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
    if edits.has_lib_rs {
        set_lib_test_false(&mut doc);
    }
    for name in &edits.bin_names {
        set_bin_test_false(&mut doc, name);
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
    if !edits.mirror_crate_idents.is_empty() {
        mirror_dev_deps_for_rudzio_test_cfg(&mut doc, &edits.mirror_crate_idents);
    }
    if edits.needs_rudzio_test_cfg {
        ensure_check_cfg_rudzio_test(&mut doc);
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

/// `[lib] test = false` suppresses cargo's default libtest "unit
/// tests" pass on the lib. Post-migration the lib has no stock
/// `#[test]` fns — they've been rewritten into `#[rudzio::test]`
/// and run via `#[rudzio::main]` — so the libtest pass is empty
/// noise. Respects an explicit `test = true` override.
fn set_lib_test_false(doc: &mut DocumentMut) {
    let lib = doc
        .as_table_mut()
        .entry("lib")
        .or_insert(Item::Table(Table::new()));
    let Some(lib_tbl) = lib.as_table_mut() else {
        return;
    };
    let user_opted_in = lib_tbl
        .get("test")
        .and_then(|v| v.as_value())
        .and_then(toml_edit::Value::as_bool)
        .is_some_and(|b| b);
    if user_opted_in {
        return;
    }
    let _prev = lib_tbl.insert("test", value(false));
}

/// Ensure `[[bin]] name = "<name>"` has `test = false`. If an entry
/// already exists for that name, amend it in place; otherwise append
/// a minimal `name + test` entry. Cargo merges these with
/// auto-discovered bins (keyed by name), so we don't need to specify
/// `path`.
fn set_bin_test_false(doc: &mut DocumentMut, bin_name: &str) {
    let bins_item = doc
        .as_table_mut()
        .entry("bin")
        .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let Some(arr) = bins_item.as_array_of_tables_mut() else {
        return;
    };
    for existing in arr.iter_mut() {
        let name_match = existing
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == bin_name);
        if name_match {
            let already_false = existing
                .get("test")
                .and_then(|v| v.as_value())
                .and_then(toml_edit::Value::as_bool)
                .is_some_and(|b| !b);
            if !already_false {
                let _prev = existing.insert("test", value(false));
            }
            return;
        }
    }
    let mut tbl = Table::new();
    let _prev_name = tbl.insert("name", value(bin_name.to_owned()));
    let _prev_test = tbl.insert("test", value(false));
    arr.push(tbl);
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

/// Ensure `[lints.rust] unexpected_cfgs = { level = "warn", check-cfg
/// = ['cfg(rudzio_test)'] }` is present. Merge-safe: if `[lints.rust]`
/// already exists, only the `check-cfg` array is touched (and only
/// to add the `cfg(rudzio_test)` entry if it's missing). `level` is
/// added only when `unexpected_cfgs` didn't already exist — we never
/// override a user-chosen severity.
fn ensure_check_cfg_rudzio_test(doc: &mut DocumentMut) {
    const CFG_ENTRY: &str = "cfg(rudzio_test)";
    let lints_was_absent = !doc.as_table().contains_key("lints");
    let lints = doc
        .as_table_mut()
        .entry("lints")
        .or_insert(Item::Table(Table::new()));
    let Some(lints_tbl) = lints.as_table_mut() else {
        return;
    };
    if lints_was_absent {
        // Avoid emitting a bare `[lints]` header before `[lints.rust]`.
        // toml_edit renders implicit parent tables without their own
        // header, which is what the user would normally write by hand.
        lints_tbl.set_implicit(true);
    }
    let rust = lints_tbl
        .entry("rust")
        .or_insert(Item::Table(Table::new()));
    let Some(rust_tbl) = rust.as_table_mut() else {
        return;
    };
    let existed = rust_tbl.contains_key("unexpected_cfgs");
    let unexpected = rust_tbl
        .entry("unexpected_cfgs")
        .or_insert(Item::Value(toml_edit::Value::InlineTable({
            let mut t = InlineTable::new();
            let _lvl = t.insert("level", "warn".into());
            let mut arr = Array::new();
            arr.push(CFG_ENTRY);
            let _cc = t.insert("check-cfg", arr.into());
            t
        })));
    if !existed {
        return;
    }
    let inline = match unexpected {
        Item::Value(toml_edit::Value::InlineTable(t)) => Some(t),
        _ => None,
    };
    let Some(inline) = inline else {
        return;
    };
    let check_cfg_item = inline
        .entry("check-cfg")
        .or_insert(toml_edit::Value::Array(Array::new()));
    if let toml_edit::Value::Array(arr) = check_cfg_item {
        let already = arr
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s == CFG_ENTRY));
        if !already {
            arr.push(CFG_ENTRY);
        }
    }
}

/// Copy `[dev-dependencies]` into `[target.'cfg(rudzio_test)'.dependencies]`
/// so `cargo rudzio test` can compile a member crate's in-src suites.
///
/// **Why:** `[dev-dependencies]` only activate when cargo builds a test
/// target (integrated `cargo test` on that crate). `cargo rudzio test`
/// builds the aggregator binary, which depends on each member as a plain
/// lib — no `--test` flag, no dev-deps — so `use ::rudzio::…` inside a
/// `#[cfg(any(test, rudzio_test))] mod tests` in `src/**` fails to
/// resolve. Mirroring into `[target.'cfg(rudzio_test)'.dependencies]`
/// makes cargo activate those deps ONLY when the `rudzio_test` cfg is
/// set (which the aggregator passes via `RUSTFLAGS=--cfg rudzio_test`),
/// so regular `cargo build` / `cargo test` / published-crate consumers
/// see no extra deps.
///
/// Idempotent: existing `[target.'cfg(rudzio_test)'.dependencies]`
/// entries are preserved and only missing names are inserted.
/// The Rust ident a dep is referenced by in source: the Cargo.toml
/// key with hyphens normalised to underscores, unless the user
/// renamed the package via `package = "…"` (in which case the key
/// itself IS the source ident — hyphens and all, with the same
/// `-` → `_` normalisation). Cargo does the same normalisation.
fn dep_rust_ident(cargo_key: &str, item: &Item) -> String {
    // `package = "…"` means the key is the user's chosen ident; the
    // original crate name (in `package`) is only for cargo's registry
    // lookup. Either way the ident comes from the KEY with `-` → `_`.
    let _ = item; // retained for future expansion; package-rename already lives on key
    cargo_key.replace('-', "_")
}

fn mirror_dev_deps_for_rudzio_test_cfg(
    doc: &mut DocumentMut,
    wanted_idents: &std::collections::BTreeSet<String>,
) {
    const CFG_KEY: &str = "cfg(rudzio_test)";

    let snapshot: Vec<(String, Item)> = doc
        .as_table()
        .get("dev-dependencies")
        .and_then(Item::as_table)
        .map(|tbl| {
            tbl.iter()
                .filter(|(name, item)| {
                    wanted_idents.contains(&dep_rust_ident(name, item))
                })
                .map(|(name, item)| (name.to_owned(), item.clone()))
                .collect()
        })
        .unwrap_or_default();
    if snapshot.is_empty() {
        return;
    }

    let target_absent = !doc.as_table().contains_key("target");
    let target = doc
        .as_table_mut()
        .entry("target")
        .or_insert(Item::Table(Table::new()));
    let Some(target_tbl) = target.as_table_mut() else {
        return;
    };
    if target_absent {
        // `[target]` itself is never rendered; the actual header is
        // `[target.'cfg(rudzio_test)'.dependencies]`.
        target_tbl.set_implicit(true);
    }

    let cfg_absent = !target_tbl.contains_key(CFG_KEY);
    let cfg_entry = target_tbl
        .entry(CFG_KEY)
        .or_insert(Item::Table(Table::new()));
    let Some(cfg_tbl) = cfg_entry.as_table_mut() else {
        return;
    };
    if cfg_absent {
        cfg_tbl.set_implicit(true);
    }

    let deps_item = cfg_tbl
        .entry("dependencies")
        .or_insert(Item::Table(Table::new()));
    let Some(deps_tbl) = deps_item.as_table_mut() else {
        return;
    };

    for (name, item) in snapshot {
        if !deps_tbl.contains_key(&name) {
            let _prev = deps_tbl.insert(&name, item);
        }
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
