//! Behavioral spec: the dep-table parser fails loudly on
//! unrecognised keys, instead of silently dropping them.
//!
//! # Background
//!
//! `apply_dev_dep_field` / `apply_ws_dep_field` used to have a
//! catch-all `_ => {}` arm that silently dropped any key the parser
//! didn't recognise. This shipped Bug #6 — `registry = "fthm"` on
//! consumer deps vanished from the generated aggregator `Cargo.toml`
//! without trace, and the failure surfaced as a downstream
//! "unresolved import" several layers removed from the actual cause.
//!
//! Strict-parse converts every silent drop into a clear error,
//! including the dep name and the offending key. New cargo dep-table
//! keys then surface immediately when a cargo-rudzio user upgrades
//! cargo, prompting an explicit update rather than another silent
//! attribution loss.
//!
//! # Behavioral assertion
//!
//! For every dep-table variant (member `[dependencies]`,
//! `[dev-dependencies]`, `[workspace.dependencies]`,
//! `[target.'cfg(unix)'.dependencies]`) inject an unrecognised key
//! (`mystery_key = "x"`) into the entry and confirm
//! `plan_from_manifest` returns an `Err` whose message names both
//! the dep and the key. Wrong-TYPE assertions are paired with the
//! unrecognised-key one — `version = 42` (integer) must also error,
//! since silent type-mismatch is the same bug class.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the rudzio repo (so synthetic fixtures can
/// `path = <this>` rudzio).
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a workspace where the consumer's `[dependencies]`
/// table carries an unrecognised key. `cargo metadata` itself
/// rejects truly unknown KEYS at the package level, but the
/// dep-table key set is forwarded unchecked, so a typo (or a new
/// cargo feature key cargo-rudzio doesn't know about) lands here and
/// would previously be silently dropped.
fn write_fixture_with_unknown_dep_key(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dependencies]\n\
         some_dep = { version = \"=1.0.0\", mystery_key = \"x\" }\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.104\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    Ok(())
}

/// Materialise a workspace where `[workspace.dependencies]` carries
/// an unrecognised key.
fn write_fixture_with_unknown_ws_key(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n\
             some_dep = {{ version = \"=1.0.0\", mystery_key = \"x\" }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.104\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    Ok(())
}

/// Materialise a workspace where a known dep key has the wrong TOML
/// type (`version = 42` instead of a string).
fn write_fixture_with_type_mismatch(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dependencies]\n\
         some_dep = { version = 42 }\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.104\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard that restores `CARGO_NET_OFFLINE` to its previous
    /// value when dropped.
    struct Restore {
        /// Value observed before the guard installed; `None` = was unset.
        prev: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                #[expect(
                    unsafe_code,
                    reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see module-level comment.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see module-level comment.
                unsafe {
                    env::remove_var("CARGO_NET_OFFLINE");
                }
            }
        }
    }
    let prev = env::var("CARGO_NET_OFFLINE").ok();
    let _guard = Restore { prev };
    #[expect(
        unsafe_code,
        reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
    )]
    // SAFETY: see Restore::drop.
    unsafe {
        env::set_var("CARGO_NET_OFFLINE", "true");
    }
    body()
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        rudzio_source_root, with_cargo_offline, write_fixture_with_type_mismatch,
        write_fixture_with_unknown_dep_key, write_fixture_with_unknown_ws_key,
    };
    use cargo_rudzio::generate::plan_from_manifest;
    use tempfile::tempdir;

    #[rudzio::test]
    fn unknown_key_in_member_dependency_returns_error() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture_with_unknown_dep_key(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let result = with_cargo_offline(|| plan_from_manifest(&manifest));
        let Err(err) = result else {
            anyhow::bail!("unknown key must bail; got Ok");
        };
        let chain = format!("{err:?}");
        anyhow::ensure!(
            chain.contains("mystery_key"),
            "diagnostic must name the unknown key, got:\n{chain}",
        );
        anyhow::ensure!(
            chain.contains("some_dep"),
            "diagnostic must name the dep that carries the unknown key, got:\n{chain}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn unknown_key_in_workspace_dependencies_returns_error() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture_with_unknown_ws_key(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let result = with_cargo_offline(|| plan_from_manifest(&manifest));
        let Err(err) = result else {
            anyhow::bail!("unknown key in workspace dep must bail; got Ok");
        };
        let chain = format!("{err:?}");
        anyhow::ensure!(
            chain.contains("mystery_key"),
            "diagnostic must name the unknown key, got:\n{chain}",
        );
        anyhow::ensure!(
            chain.contains("some_dep"),
            "diagnostic must name the dep that carries the unknown key, got:\n{chain}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn type_mismatch_on_known_key_returns_error() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture_with_type_mismatch(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let result = with_cargo_offline(|| plan_from_manifest(&manifest));
        let Err(err) = result else {
            anyhow::bail!("type mismatch on known key must bail; got Ok");
        };
        let chain = format!("{err:?}");
        anyhow::ensure!(
            chain.contains("version") && chain.contains("string"),
            "diagnostic must name the offending key and the expected type, got:\n{chain}",
        );
        anyhow::ensure!(
            chain.contains("some_dep"),
            "diagnostic must name the dep, got:\n{chain}",
        );
        Ok(())
    }
}
