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
//! The legacy extensionless `.cargo/config` is preferred over
//! `config.toml` when both exist in the same directory (mirroring cargo's
//! warning), and `--config` command-line overrides are applied as the
//! highest-precedence layer: the string form `env.KEY="value"` and the
//! file form `--config <path>`. Cargo rejects the inline-table form in
//! `--config`, so it is rejected here too.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use toml_edit::{DocumentMut, Item, Value};

#[cfg(any(test, rudzio_test))]
use rudzio::common::context::{Suite, Test};
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::futures::ThreadPool;
#[cfg(all(any(test, rudzio_test), any(target_os = "linux", target_os = "macos"),))]
use rudzio::runtime::monoio;
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::{async_std, compio, embassy, smol};

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
/// `cli_config` is the list of `--config` argument values (the payload
/// after the flag, already split from `--config`/`--config=`), each either
/// a `KEY=VALUE` pair or a path to a config file.
///
/// Layering, lowest precedence first:
///
/// 1. `$CARGO_HOME/config` (`$HOME/.cargo/config` when unset), with the
///    extensionless `config` preferred over `config.toml` when both exist.
/// 2. `.cargo/config` files walking up from `workspace_root` to the
///    filesystem root, farthest ancestor first so the closest directory
///    wins per key. Again, extensionless `config` beats `config.toml`.
/// 3. `--config` command-line overrides (highest precedence): the string
///    form `env.KEY="value"` and the file form `--config <path>`.
///
/// Each layer's `[env]` table is merged with closer keys overriding farther
/// ones (value *and* `force`), mirroring cargo's per-key merge.
///
/// # Errors
///
/// Fails when a config file cannot be read or parsed, when `[env]` is not a
/// table, when an entry has an unsupported shape, type, or key, or when a
/// `--config env.KEY=...` value is not a plain TOML string.
#[inline]
pub fn resolve(workspace_root: &Path, cli_config: &[String]) -> Result<BTreeMap<String, EnvVar>> {
    let mut merged: BTreeMap<String, EnvVar> = BTreeMap::new();

    // Layer 1 (lowest): `$CARGO_HOME/config(.toml)`. `relative` paths there
    // resolve against the parent of `$CARGO_HOME` (cargo treats CARGO_HOME
    // as the `.cargo` directory, so "parent of `.cargo`" is its parent).
    if let Some(home) = cargo_home()
        && let Some(config) = discover_config_file(&home)
    {
        let base = home.parent().map(Path::to_path_buf).unwrap_or_default();
        merge_file(&mut merged, &config, &base)?;
    }

    // Layer 2..N: `.cargo/config(.toml)` ancestors, farthest first so the
    // closest dir (workspace root) is applied last and wins.
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(workspace_root);
    while let Some(ancestor) = cursor {
        chain.push(ancestor.to_path_buf());
        cursor = ancestor.parent();
    }
    for config_dir in chain.iter().rev() {
        let cargo_dir = config_dir.join(".cargo");
        if let Some(config) = discover_config_file(&cargo_dir) {
            // `relative` resolves against the parent of the `.cargo` dir.
            merge_file(&mut merged, &config, config_dir)?;
        }
    }

    // Layer 3 (highest): `--config` command-line overrides.
    merge_cli_config(&mut merged, cli_config, workspace_root)?;

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

/// Write `text` to stderr, ignoring any I/O error.
///
/// The config-discovery ambiguity warning is advisory: a failed write must
/// not turn a valid resolution into an error.
fn warn(text: &str) {
    use std::io::{self, Write as _};
    let _io_result: io::Result<()> = io::stderr().lock().write_all(text.as_bytes());
}

/// Choose the config file cargo reads in `cargo_dir` (a `.cargo` directory
/// or `$CARGO_HOME`).
///
/// Cargo prefers the legacy extensionless `config` over `config.toml` and,
/// when both exist, warns that the extensionless file wins.
fn discover_config_file(cargo_dir: &Path) -> Option<PathBuf> {
    let extensionless = cargo_dir.join("config");
    if extensionless.is_file() {
        let toml = cargo_dir.join("config.toml");
        if toml.is_file() {
            warn(&format!(
                "warning: both `{}` and `{}` exist. Using `{}`\n",
                extensionless.display(),
                toml.display(),
                extensionless.display(),
            ));
        }
        return Some(extensionless);
    }
    let toml = cargo_dir.join("config.toml");
    toml.is_file().then_some(toml)
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

/// Apply `--config` command-line overrides (cargo's highest-precedence
/// layer) to `merged`.
///
/// Each value is either a `KEY=VALUE` pair or a path to a config file,
/// mirroring cargo's `--config <KEY=VALUE|PATH>` grammar. Only `env.*`
/// keys shape the runner environment; other keys (e.g. `build.rustflags`)
/// are cargo's own concern and are already forwarded to the inner build.
///
/// A relative file path resolves against `workspace_root`, the CWD cargo's
/// inner build is anchored at.
fn merge_cli_config(
    merged: &mut BTreeMap<String, EnvVar>,
    cli_config: &[String],
    workspace_root: &Path,
) -> Result<()> {
    for value in cli_config {
        if let Some((key, raw)) = value.split_once('=') {
            let trimmed_key = key.trim();
            if let Some(name) = trimmed_key.strip_prefix("env.") {
                if name.is_empty() {
                    bail!("`--config {value}` sets an empty `[env]` variable name");
                }
                if name.contains('.') {
                    bail!(
                        "`--config {value}` uses a nested `[env]` sub-key, which is not \
                         supported via `--config`; use the string form `env.NAME=\"value\"` \
                         or a `--config <path>` file"
                    );
                }
                let var = parse_cli_env_value(name, raw.trim())?;
                let _previous = merged.insert(name.to_owned(), var);
            }
            // A non-`env.*` `--config` key does not shape the runner env.
        } else {
            let path = Path::new(value);
            let abs = if path.is_absolute() {
                path.to_path_buf()
            } else {
                workspace_root.join(path)
            };
            // `relative` paths inside a `--config <path>` file resolve two
            // levels up from the file itself (cargo's documented rule).
            let base = abs
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            merge_file(merged, &abs, &base)
                .with_context(|| format!("loading `--config` file {value}"))?;
        }
    }
    Ok(())
}

/// Parse a `--config env.NAME=<toml-value>` payload into an [`EnvVar`].
///
/// Cargo's `--config` accepts only scalar TOML values, so the only valid
/// `[env]` form here is a quoted string (`env.NAME="value"`). The inline
/// table (`env.NAME={ value = .., force = .. }`) and any non-string scalar
/// are rejected with a hard error, matching cargo; `force`/`relative` are
/// only expressible through config files.
fn parse_cli_env_value(name: &str, raw: &str) -> Result<EnvVar> {
    let doc: DocumentMut = format!("value = {raw}").parse().map_err(|err| {
        anyhow::anyhow!("`--config env.{name}=...` value `{raw}` is not valid TOML: {err}")
    })?;
    let item = doc.get("value").ok_or_else(|| {
        anyhow::anyhow!("`--config env.{name}=...` value `{raw}` produced no value")
    })?;
    match item {
        Item::Value(Value::String(text)) => Ok(EnvVar {
            force: false,
            value: text.value().clone(),
        }),
        Item::Value(Value::InlineTable(_)) => bail!(
            "--config argument `env.{name}={raw}` sets a value to an inline table, which is not accepted"
        ),
        rejected @ (Item::None
        | Item::Value(
            Value::Integer(_)
            | Value::Float(_)
            | Value::Boolean(_)
            | Value::Datetime(_)
            | Value::Array(_),
        )
        | Item::Table(_)
        | Item::ArrayOfTables(_)) => {
            bail!("--config argument `env.{name}={raw}` must be a string, got {rejected}")
        }
    }
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

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
#[cfg(any(test, rudzio_test))]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use toml_edit::DocumentMut;

    use super::{
        EnvVar, Test, discover_config_file, merge_cli_config, merge_file, parse_cli_env_value,
        parse_entry,
    };

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

    #[rudzio::test]
    async fn plain_string_entry(_ctx: &Test) -> anyhow::Result<()> {
        assert_eq!(
            entry("MY_VAR = \"consumer\"", "MY_VAR")?,
            EnvVar {
                value: "consumer".to_owned(),
                force: false,
            }
        );
        Ok(())
    }

    #[rudzio::test]
    async fn inline_table_force(_ctx: &Test) -> anyhow::Result<()> {
        assert_eq!(
            entry("TOKEN = { value = \"secret\", force = true }", "TOKEN")?,
            EnvVar {
                value: "secret".to_owned(),
                force: true,
            }
        );
        Ok(())
    }

    #[rudzio::test]
    async fn inline_table_relative_resolves_against_parent_of_cargo_dir(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
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

    #[rudzio::test]
    async fn inline_table_unknown_key_fails_loudly(_ctx: &Test) -> anyhow::Result<()> {
        let err = entry("MY_VAR = { value = \"x\", bogus = true }", "MY_VAR")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected an error for an unknown key"))?;
        assert!(
            format!("{err:#}").contains("unknown key `bogus`"),
            "{err:#}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn inline_table_missing_value_fails_loudly(_ctx: &Test) -> anyhow::Result<()> {
        let err = entry("MY_VAR = { force = true }", "MY_VAR")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected an error for a missing value"))?;
        assert!(format!("{err:#}").contains("must set `value`"), "{err:#}");
        Ok(())
    }

    #[rudzio::test]
    async fn non_string_non_table_fails_loudly(_ctx: &Test) -> anyhow::Result<()> {
        let err = entry("MY_VAR = 1", "MY_VAR")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected an error for an integer"))?;
        assert!(
            format!("{err:#}").contains("must be a string or an inline table"),
            "{err:#}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn later_file_overrides_value_and_force_per_key(_ctx: &Test) -> anyhow::Result<()> {
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

    #[rudzio::test]
    async fn discover_config_file_prefers_extensionless(_ctx: &Test) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("config"), "[env]\nA = \"ext\"\n")?;
        fs::write(dir.path().join("config.toml"), "[env]\nA = \"toml\"\n")?;
        let chosen = discover_config_file(dir.path())
            .ok_or_else(|| anyhow::anyhow!("expected an extensionless config"))?;
        assert_eq!(chosen, dir.path().join("config"));
        Ok(())
    }

    #[rudzio::test]
    async fn discover_config_file_falls_back_to_toml(_ctx: &Test) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("config.toml"), "[env]\nA = \"toml\"\n")?;
        let chosen = discover_config_file(dir.path())
            .ok_or_else(|| anyhow::anyhow!("expected a config.toml"))?;
        assert_eq!(chosen, dir.path().join("config.toml"));
        Ok(())
    }

    #[rudzio::test]
    async fn discover_config_file_none_when_absent(_ctx: &Test) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(discover_config_file(dir.path()).is_none());
        Ok(())
    }

    #[rudzio::test]
    async fn cli_env_string_form(_ctx: &Test) -> anyhow::Result<()> {
        assert_eq!(
            parse_cli_env_value("FOO", "\"bar\"")?,
            EnvVar {
                value: "bar".to_owned(),
                force: false,
            }
        );
        Ok(())
    }

    #[rudzio::test]
    async fn cli_env_inline_table_rejected(_ctx: &Test) -> anyhow::Result<()> {
        let err = parse_cli_env_value("FOO", "{ value = \"bar\", force = true }")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected the inline table to be rejected"))?;
        assert!(format!("{err:#}").contains("inline table"), "{err:#}");
        Ok(())
    }

    #[rudzio::test]
    async fn cli_env_bare_value_rejected(_ctx: &Test) -> anyhow::Result<()> {
        let err = parse_cli_env_value("FOO", "bar")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected the bare value to be rejected"))?;
        assert!(format!("{err:#}").contains("not valid TOML"), "{err:#}");
        Ok(())
    }

    #[rudzio::test]
    async fn cli_env_non_string_scalar_rejected(_ctx: &Test) -> anyhow::Result<()> {
        let err = parse_cli_env_value("FOO", "42")
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected the integer to be rejected"))?;
        assert!(format!("{err:#}").contains("must be a string"), "{err:#}");
        Ok(())
    }

    #[rudzio::test]
    async fn cli_env_nested_subkey_rejected(_ctx: &Test) -> anyhow::Result<()> {
        let mut merged: BTreeMap<String, EnvVar> = BTreeMap::new();
        let err = merge_cli_config(
            &mut merged,
            &["env.FOO.value=\"bar\"".to_owned()],
            Path::new("/ws"),
        )
        .err()
        .ok_or_else(|| anyhow::anyhow!("expected the nested sub-key to be rejected"))?;
        assert!(
            format!("{err:#}").contains("nested `[env]` sub-key"),
            "{err:#}"
        );
        Ok(())
    }

    #[rudzio::test]
    async fn cli_config_overrides_file_per_key(_ctx: &Test) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("config.toml");
        fs::write(&file, "[env]\nFOO = \"file\"\nKEEP = \"kept\"\n")?;
        let mut merged: BTreeMap<String, EnvVar> = BTreeMap::new();
        merge_file(&mut merged, &file, dir.path())?;
        merge_cli_config(&mut merged, &["env.FOO=\"cli\"".to_owned()], dir.path())?;
        let expected: BTreeMap<String, EnvVar> = BTreeMap::from([
            (
                "FOO".to_owned(),
                EnvVar {
                    value: "cli".to_owned(),
                    force: false,
                },
            ),
            (
                "KEEP".to_owned(),
                EnvVar {
                    value: "kept".to_owned(),
                    force: false,
                },
            ),
        ]);
        assert_eq!(merged, expected);
        Ok(())
    }

    #[rudzio::test]
    async fn cli_config_file_form_merges_env(_ctx: &Test) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let extra = dir.path().join("extra.toml");
        fs::write(&extra, "[env]\nEXTRA = \"from-file\"\n")?;
        let mut merged: BTreeMap<String, EnvVar> = BTreeMap::new();
        merge_cli_config(
            &mut merged,
            &[extra.to_string_lossy().into_owned()],
            dir.path(),
        )?;
        let expected: BTreeMap<String, EnvVar> = BTreeMap::from([(
            "EXTRA".to_owned(),
            EnvVar {
                value: "from-file".to_owned(),
                force: false,
            },
        )]);
        assert_eq!(merged, expected);
        Ok(())
    }

    #[rudzio::test]
    async fn cli_config_ignores_non_env_key(_ctx: &Test) -> anyhow::Result<()> {
        let mut merged: BTreeMap<String, EnvVar> = BTreeMap::new();
        merge_cli_config(
            &mut merged,
            &["build.rustflags=\"--cfg foo\"".to_owned()],
            Path::new("/ws"),
        )?;
        assert!(merged.is_empty());
        Ok(())
    }
}
