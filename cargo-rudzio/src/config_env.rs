//! Resolve the consumer's effective cargo `[env]` configuration for the
//! runner process.
//!
//! # Why
//!
//! Stock cargo applies the `[env]` table from `.cargo/config.toml` to the
//! binaries it *runs* (`cargo run`, and the test binary under `cargo test`)
//! in addition to the `rustc`/build-script invocations it drives. `cargo
//! rudzio test` compiles the aggregator with a stock `cargo build` (so the
//! compile-time side works via `option_env!`), but it `exec`s the runner
//! binary directly, bypassing cargo's environment injection — so
//! `std::env::var` on a `[env]` key comes back `NotPresent` at runtime.
//! This module re-reads the same config files cargo would and re-injects the
//! resolved variables onto the spawned runner.
//!
//! # Scope
//!
//! Only the runner is re-injected. Build scripts, proc-macros, and `rustc`
//! already receive `[env]` from the stock `cargo build` cargo-rudzio spawns
//! (anchored at the consumer's workspace root), so those surfaces are
//! correct without duplication.
//!
//! Supported entry shapes: a plain string, and an inline table with
//! `value`, `force`, and `relative`. Any other shape or key is a hard error.
//! Only `config.toml` files are read (not the legacy extensionless
//! `config`), and `--config env.*` command-line overrides are not applied to
//! the runner.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use toml_edit::{DocumentMut, Item, Value};

/// A single resolved `[env]` entry, ready to apply to the runner.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EnvVar {
    /// Override an already-set variable in the runner's environment.
    pub force: bool,
    /// Variable value, already made absolute when `relative = true`.
    pub value: String,
}

/// Resolve the consumer's effective `[env]` map under cargo's config
/// precedence.
///
/// Layering, lowest precedence first:
///
/// 1. `$CARGO_HOME/config.toml` (`$HOME/.cargo/config.toml` when unset).
/// 2. `.cargo/config.toml` files walking up from `workspace_root` to the
///    filesystem root, farthest ancestor first so the closest directory
///    wins per key.
///
/// Each layer's `[env]` table is merged with closer keys overriding farther
/// ones (value *and* `force`), mirroring cargo's per-key merge.
///
/// # Errors
///
/// Fails when a config file cannot be read or parsed, when `[env]` is not a
/// table, or when an entry has an unsupported shape, type, or key.
#[inline]
pub fn resolve(workspace_root: &Path) -> Result<BTreeMap<String, EnvVar>> {
    let mut merged: BTreeMap<String, EnvVar> = BTreeMap::new();

    // Layer 1 (lowest): `$CARGO_HOME/config.toml`. `relative` paths there
    // resolve against the parent of `$CARGO_HOME` (cargo treats CARGO_HOME
    // as the `.cargo` directory, so "parent of `.cargo`" is its parent).
    if let Some(home) = cargo_home() {
        let config = home.join("config.toml");
        if config.is_file() {
            let base = home.parent().map(Path::to_path_buf).unwrap_or_default();
            merge_file(&mut merged, &config, &base)?;
        }
    }

    // Layer 2..N: `.cargo/config.toml` ancestors, farthest first so the
    // closest dir (workspace root) is applied last and wins.
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(workspace_root);
    while let Some(ancestor) = cursor {
        chain.push(ancestor.to_path_buf());
        cursor = ancestor.parent();
    }
    for config_dir in chain.iter().rev() {
        let config = config_dir.join(".cargo").join("config.toml");
        if config.is_file() {
            // `relative` resolves against the parent of the `.cargo` dir.
            merge_file(&mut merged, &config, config_dir)?;
        }
    }

    Ok(merged)
}

/// `$CARGO_HOME` from the environment, defaulting to `$HOME/.cargo`.
///
/// `None` when neither is set (no home config to read, matching cargo's
/// "no home config" behaviour when the home directory is indeterminate).
fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = env::var_os("CARGO_HOME") {
        return Some(PathBuf::from(home));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo"))
}

/// Merge the `[env]` table of a single config file into `merged`, with
/// every key in this file overriding whatever `merged` already holds.
///
/// `base` is the directory `relative = true` values resolve against (the
/// parent of the `.cargo` directory that contains `path`, or the parent of
/// `$CARGO_HOME` for the home config).
fn merge_file(merged: &mut BTreeMap<String, EnvVar>, path: &Path, base: &Path) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let Some(env_item) = doc.get("env") else {
        return Ok(());
    };
    let env_table = match env_item {
        Item::Table(table) => table,
        rejected @ (Item::None | Item::Value(_) | Item::ArrayOfTables(_)) => {
            bail!(
                "`[env]` in {} must be a table, got {rejected}",
                path.display()
            )
        }
    };
    for (name, item) in env_table {
        let var = parse_entry(item, name, base, path)?;
        let _previous = merged.insert(name.to_owned(), var);
    }
    Ok(())
}

/// Parse a single `[env]` entry into an [`EnvVar`].
///
/// Accepts cargo's two documented shapes:
///
/// * a plain string (`MY_VAR = "value"`), with `force`/`relative` defaulting
///   to `false`;
/// * an inline table (`MY_VAR = { value = "value", force = true,
///   relative = true }`).
///
/// Any other value type, a missing `value`, or an unknown table key is a
/// hard error — a silent drop would re-introduce the exact bug this module
/// exists to fix.
fn parse_entry(item: &Item, name: &str, base: &Path, path: &Path) -> Result<EnvVar> {
    match item {
        Item::Value(Value::String(text)) => Ok(EnvVar {
            force: false,
            value: text.value().clone(),
        }),
        Item::Value(Value::InlineTable(table)) => {
            let mut raw_value: Option<String> = None;
            let mut force = false;
            let mut relative = false;
            for (key, val) in table {
                match key {
                    "value" => {
                        let text = val.as_str().ok_or_else(|| {
                            anyhow::anyhow!(
                                "[env].{name}.value in {} must be a string, got {val}",
                                path.display()
                            )
                        })?;
                        raw_value = Some(text.to_owned());
                    }
                    "force" => {
                        force = val.as_bool().ok_or_else(|| {
                            anyhow::anyhow!(
                                "[env].{name}.force in {} must be a bool, got {val}",
                                path.display()
                            )
                        })?;
                    }
                    "relative" => {
                        relative = val.as_bool().ok_or_else(|| {
                            anyhow::anyhow!(
                                "[env].{name}.relative in {} must be a bool, got {val}",
                                path.display()
                            )
                        })?;
                    }
                    unknown => bail!(
                        "[env].{name} in {} has unknown key `{unknown}`",
                        path.display()
                    ),
                }
            }
            let literal = raw_value.ok_or_else(|| {
                anyhow::anyhow!("[env].{name} in {} must set `value`", path.display())
            })?;
            let resolved = if relative {
                resolve_relative(base, &literal, name, path)?
            } else {
                literal
            };
            Ok(EnvVar {
                force,
                value: resolved,
            })
        }
        rejected @ (Item::None
        | Item::Value(
            Value::Integer(_)
            | Value::Float(_)
            | Value::Boolean(_)
            | Value::Datetime(_)
            | Value::Array(_),
        )
        | Item::Table(_)
        | Item::ArrayOfTables(_)) => bail!(
            "[env].{name} in {} must be a string or an inline table, got {rejected}",
            path.display()
        ),
    }
}

/// Make `value` absolute against `base`, mirroring cargo's `relative = true`
/// resolution. The target need not exist, so this is a lexical join (no
/// canonicalization).
fn resolve_relative(base: &Path, value: &str, name: &str, path: &Path) -> Result<String> {
    if base.as_os_str().is_empty() {
        bail!(
            "[env].{name} in {} is `relative = true` but has no config directory to resolve against",
            path.display()
        );
    }
    Ok(base.join(value).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use toml_edit::DocumentMut;

    use super::{EnvVar, merge_file, parse_entry};

    const CONFIG: &str = "/ws/.cargo/config.toml";

    /// Parse `[env]\n<line>` and run [`parse_entry`] on `name`.
    fn entry(line: &str, name: &str) -> anyhow::Result<EnvVar> {
        let doc: DocumentMut = format!("[env]\n{line}\n").parse()?;
        let item = doc
            .get("env")
            .and_then(|env| env.get(name))
            .ok_or_else(|| anyhow::anyhow!("missing {name}"))?;
        parse_entry(item, name, Path::new("/ws"), Path::new(CONFIG))
    }

    #[test]
    fn plain_string_entry() -> anyhow::Result<()> {
        assert_eq!(
            entry("MY_VAR = \"consumer\"", "MY_VAR")?,
            EnvVar {
                value: "consumer".to_owned(),
                force: false,
            }
        );
        Ok(())
    }

    #[test]
    fn inline_table_force() -> anyhow::Result<()> {
        assert_eq!(
            entry("TOKEN = { value = \"secret\", force = true }", "TOKEN")?,
            EnvVar {
                value: "secret".to_owned(),
                force: true,
            }
        );
        Ok(())
    }

    #[test]
    fn inline_table_relative_resolves_against_parent_of_cargo_dir() -> anyhow::Result<()> {
        assert_eq!(
            entry(
                "OPENSSL_DIR = { value = \"vendor/openssl\", relative = true }",
                "OPENSSL_DIR"
            )?,
            EnvVar {
                value: "/ws/vendor/openssl".to_owned(),
                force: false,
            }
        );
        Ok(())
    }

    #[test]
    fn inline_table_unknown_key_fails_loudly() -> anyhow::Result<()> {
        let err = entry("MY_VAR = { value = \"x\", bogus = true }", "MY_VAR")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected an error for an unknown key"))?;
        assert!(
            format!("{err:#}").contains("unknown key `bogus`"),
            "{err:#}"
        );
        Ok(())
    }

    #[test]
    fn inline_table_missing_value_fails_loudly() -> anyhow::Result<()> {
        let err = entry("MY_VAR = { force = true }", "MY_VAR")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected an error for a missing value"))?;
        assert!(format!("{err:#}").contains("must set `value`"), "{err:#}");
        Ok(())
    }

    #[test]
    fn non_string_non_table_fails_loudly() -> anyhow::Result<()> {
        let err = entry("MY_VAR = 1", "MY_VAR")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected an error for an integer"))?;
        assert!(
            format!("{err:#}").contains("must be a string or an inline table"),
            "{err:#}"
        );
        Ok(())
    }

    #[test]
    fn later_file_overrides_value_and_force_per_key() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let low = dir.path().join("low.toml");
        let high = dir.path().join("high.toml");
        fs::write(
            &low,
            "[env]\nA = { value = \"low\", force = true }\nB = \"low-b\"\n",
        )?;
        fs::write(&high, "[env]\nA = \"high\"\n")?;
        let mut merged: BTreeMap<String, EnvVar> = BTreeMap::new();
        merge_file(&mut merged, &low, dir.path())?;
        merge_file(&mut merged, &high, dir.path())?;
        let expected: BTreeMap<String, EnvVar> = BTreeMap::from([
            (
                "A".to_owned(),
                EnvVar {
                    value: "high".to_owned(),
                    force: false,
                },
            ),
            (
                "B".to_owned(),
                EnvVar {
                    value: "low-b".to_owned(),
                    force: false,
                },
            ),
        ]);
        assert_eq!(merged, expected);
        Ok(())
    }
}
