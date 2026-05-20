//! Behavioral spec: `registry = "<name>"` attribution on dep entries
//! survives the full aggregator-generation pipeline.
//!
//! # Symptom (pre-fix)
//!
//! In a consumer workspace where `[dependencies]`, `[dev-dependencies]`,
//! `[target.'cfg(...)'.dependencies]`, and / or `[workspace.dependencies]`
//! declare entries with `registry = "fthm"` (an alternative registry,
//! configured in `.cargo/config.toml`), every `cargo rudzio test` run
//! regenerates `target/rudzio-auto-runner/Cargo.toml` and the
//! `members/<member>/Cargo.toml` bridge crates **without** the
//! `registry = "..."` keys. Cargo then tries to resolve those deps from
//! crates.io and fails, breaking the consumer's build under cargo-rudzio
//! while leaving plain `cargo test` (which preserves the original
//! Cargo.toml verbatim) working — a confusing asymmetric failure.
//!
//! # Root cause
//!
//! `cargo_rudzio::generate`'s dep parser
//! (`apply_dev_dep_field` / `apply_ws_dep_field`) and emitter
//! (`render_dev_dep` / `build_rudzio_inline_table`) have no `"registry"`
//! arm. The key is parsed but silently dropped on the `_ => {}` arm,
//! then never re-emitted into the generated TOML.
//!
//! # Behavioral assertion
//!
//! The fixture below synthesizes a single-member workspace where every
//! dep-table variant cargo supports carries `registry = "fthm"` on at
//! least one entry, plus rudzio itself declared with
//! `registry = "fthm"`. After driving the pipeline through
//! `plan_from_manifest` + `write_runner` (the same entry points
//! `cargo rudzio test` uses), the test reads back the generated
//! aggregator `Cargo.toml` and bridge `Cargo.toml` from disk and asserts
//! every registry-tagged dep retains its `registry = "fthm"` key.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::async_std::Runtime as AsyncStdRuntime;
use rudzio::runtime::compio::Runtime as CompioRuntime;
use rudzio::runtime::embassy::Runtime as EmbassyRuntime;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;
use rudzio::runtime::smol::Runtime as SmolRuntime;
use rudzio::runtime::tokio::{
    CurrentThread as TokioCurrentThread, Local as TokioLocal, Multithread as TokioMultithread,
};
use tempfile::TempDir;

/// Workspace root of the rudzio repo this test lives in.
///
/// `cargo-rudzio`'s manifest dir is `<workspace_root>/cargo-rudzio`;
/// joining `..` resolves to the workspace root where the `rudzio`
/// crate itself lives. Synthetic fixtures reference `rudzio` by
/// `path = <this>` so the test exercises the in-tree code, not the
/// published version. `rudzio::manifest_dir!()` (not bare
/// `env!("CARGO_MANIFEST_DIR")`) is required because this test file
/// is `#[path]`-included into the cargo-rudzio aggregator crate when
/// run via `cargo rudzio test`.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a synthetic single-member workspace at `root` with
/// every dep-table variant cargo supports carrying at least one entry
/// tagged `registry = "fthm"`, plus an `[registries.fthm]` entry in
/// `.cargo/config.toml` so `cargo metadata` accepts the registry name
/// without trying to reach the (intentionally invalid) sparse URL.
///
/// The consumer crate is a `lib + #[rudzio::test]` so cargo-rudzio
/// recognises it as a bridge-eligible member; `rudzio` itself is
/// pulled in via `path = <rudzio_root>` rather than from `fthm` so the
/// generator can resolve the in-tree code while the registry-tagged
/// deps remain unresolved (cargo metadata `--no-deps` is enough).
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::create_dir_all(root.join(".cargo"))?;
    // `index = "sparse+https://example.invalid/"` is never reached:
    // `cargo metadata --no-deps` validates the registry name against
    // the config but does not fetch the index. `CARGO_NET_OFFLINE`
    // (set by the test body) is belt-and-suspenders.
    fs::write(
        root.join(".cargo").join("config.toml"),
        "[registries.fthm]\n\
         index = \"sparse+https://example.invalid/\"\n",
    )?;

    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n\
             ws_dep_via_registry = {{ version = \"=1.0.0\", registry = \"fthm\" }}\n",
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
         rudzio = { workspace = true }\n\
         direct_dep_via_registry = { version = \"=3.0.0\", registry = \"fthm\" }\n\
         ws_dep_via_registry = { workspace = true }\n\
         \n\
         [dev-dependencies]\n\
         dev_dep_via_registry = { version = \"=4.0.0\", registry = \"fthm\" }\n\
         anyhow = \"=1.0.102\"\n\
         \n\
         [target.'cfg(unix)'.dependencies]\n\
         target_cfg_dep_via_registry = { version = \"=5.0.0\", registry = \"fthm\" }\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn placeholder() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

/// Same as [`write_fixture`] but rudzio itself is declared with
/// `version = "..." + registry = "fthm"` instead of `path = ...`.
/// Exercises the `RudzioLocation::Version` emit path with a registry.
///
/// Cargo metadata `--no-deps` accepts the unresolved rudzio dep
/// (it parses the manifest but does not resolve sources), so the
/// generator's plan-builder still produces a usable [`Plan`].
fn write_fixture_with_rudzio_on_registry(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo").join("config.toml"),
        "[registries.fthm]\n\
         index = \"sparse+https://example.invalid/\"\n",
    )?;

    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\n\
         members = [\"consumer\"]\n\
         resolver = \"3\"\n",
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
         rudzio = { version = \"=0.1.0\", registry = \"fthm\", features = [\"common\", \"runtime-tokio-multi-thread\"] }\n\
         anyhow = \"=1.0.102\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;

    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`, restoring
/// the previous value on exit. `cargo metadata` consults this var to
/// avoid touching the network when resolving registry indexes — even
/// with `--no-deps`, cargo will eagerly refresh sparse indexes the
/// first time a registry name is mentioned, which would hang the test
/// on `example.invalid`.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard that restores `CARGO_NET_OFFLINE` to its previous
    /// value when dropped. Survives panics in `body`.
    struct Restore {
        /// The value of `CARGO_NET_OFFLINE` observed before the
        /// guard was installed. `None` means the variable was unset.
        prev: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                // SAFETY: setting env vars is not unsafe per se on Unix
                // / Windows, but `std::env::set_var` is marked unsafe
                // since Rust 1.92 to discourage racy use. This test
                // runs single-threaded with respect to env vars — the
                // RAII drop runs on the same thread that set it — so
                // no race exists.
                #[expect(
                    unsafe_code,
                    reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see above.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see above.
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

/// Parse a TOML file at `path` into a `BTreeMap<String, ...>` keyed by
/// dep name, with each value being the full inline-table contents
/// rendered as a single line. Used by the assertions to inspect a
/// specific dep entry without grep-counting.
fn read_dependencies_section(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Track [dependencies] tables: top-level `[dependencies]`,
            // `[target.'cfg(...)'.dependencies]`, etc. — anything ending
            // in `.dependencies]` or being literally `[dependencies]`.
            in_deps = trimmed == "[dependencies]"
                || trimmed.ends_with(".dependencies]")
                || trimmed.ends_with(".dev-dependencies]")
                || trimmed == "[dev-dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }
        if let Some(eq_idx) = trimmed.find('=') {
            let Some(raw_name) = trimmed.get(..eq_idx) else {
                continue;
            };
            let name = raw_name.trim().to_owned();
            let Some(rest) = trimmed.get(eq_idx.saturating_add(1)..) else {
                continue;
            };
            let _previous = out.insert(name, rest.trim().to_owned());
        }
    }
    Ok(out)
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
    (runtime = TokioCurrentThread::new, suite = Suite, test = Test),
    (runtime = TokioLocal::new, suite = Suite, test = Test),
    (runtime = CompioRuntime::new, suite = Suite, test = Test),
    (runtime = EmbassyRuntime::new, suite = Suite, test = Test),
    (runtime = FuturesThreadPool::new, suite = Suite, test = Test),
    (runtime = AsyncStdRuntime::new, suite = Suite, test = Test),
    (runtime = SmolRuntime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        Path, PathBuf, TempDir, fs, plan_from_manifest, read_dependencies_section,
        rudzio_source_root, with_cargo_offline, write_fixture,
        write_fixture_with_rudzio_on_registry, write_runner,
    };

    /// Helper: synthesize the fixture, drive `plan_from_manifest` +
    /// `write_runner`, and return `(workspace_root, out_dir)` so the
    /// test bodies can read generated files. `setup` writes the
    /// per-fixture `Cargo.toml` shape; the rest of the pipeline is
    /// identical across fixtures.
    fn drive_pipeline<F>(setup: F) -> anyhow::Result<(TempDir, PathBuf)>
    where
        F: FnOnce(&Path) -> super::io::Result<()>,
    {
        let temp = tempfile::tempdir()?;
        setup(temp.path())?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        Ok((temp, out_dir))
    }

    #[rudzio::test]
    fn aggregator_preserves_registry_on_direct_member_dependency() -> anyhow::Result<()> {
        let rudzio_root = rudzio_source_root();
        let (_temp, out_dir) = drive_pipeline(|root| write_fixture(root, &rudzio_root))?;
        let aggregator = out_dir.join("Cargo.toml");
        let deps = read_dependencies_section(&aggregator)?;
        let entry = deps.get("direct_dep_via_registry").ok_or_else(|| {
            anyhow::anyhow!(
                "direct_dep_via_registry missing from aggregator Cargo.toml:\n{}",
                fs::read_to_string(&aggregator).unwrap_or_default()
            )
        })?;
        anyhow::ensure!(
            entry.contains("registry = \"fthm\""),
            "direct_dep_via_registry must keep registry = \"fthm\" in aggregator, got: {entry}\n\nfull file:\n{}",
            fs::read_to_string(&aggregator).unwrap_or_default()
        );
        Ok(())
    }

    #[rudzio::test]
    fn aggregator_preserves_registry_on_workspace_inherited_dependency() -> anyhow::Result<()> {
        let rudzio_root = rudzio_source_root();
        let (_temp, out_dir) = drive_pipeline(|root| write_fixture(root, &rudzio_root))?;
        let aggregator = out_dir.join("Cargo.toml");
        let deps = read_dependencies_section(&aggregator)?;
        let entry = deps.get("ws_dep_via_registry").ok_or_else(|| {
            anyhow::anyhow!(
                "ws_dep_via_registry missing from aggregator Cargo.toml:\n{}",
                fs::read_to_string(&aggregator).unwrap_or_default()
            )
        })?;
        anyhow::ensure!(
            entry.contains("registry = \"fthm\""),
            "workspace-inherited dep must materialise registry = \"fthm\" in aggregator, got: {entry}\n\nfull file:\n{}",
            fs::read_to_string(&aggregator).unwrap_or_default()
        );
        Ok(())
    }

    #[rudzio::test]
    fn aggregator_preserves_registry_on_dev_dependency() -> anyhow::Result<()> {
        let rudzio_root = rudzio_source_root();
        let (_temp, out_dir) = drive_pipeline(|root| write_fixture(root, &rudzio_root))?;
        let aggregator = out_dir.join("Cargo.toml");
        let deps = read_dependencies_section(&aggregator)?;
        let entry = deps.get("dev_dep_via_registry").ok_or_else(|| {
            anyhow::anyhow!(
                "dev_dep_via_registry missing from aggregator Cargo.toml:\n{}",
                fs::read_to_string(&aggregator).unwrap_or_default()
            )
        })?;
        anyhow::ensure!(
            entry.contains("registry = \"fthm\""),
            "dev-dep must keep registry = \"fthm\" in aggregator, got: {entry}\n\nfull file:\n{}",
            fs::read_to_string(&aggregator).unwrap_or_default()
        );
        Ok(())
    }

    #[rudzio::test]
    fn aggregator_preserves_registry_on_target_cfg_dependency() -> anyhow::Result<()> {
        let rudzio_root = rudzio_source_root();
        let (_temp, out_dir) = drive_pipeline(|root| write_fixture(root, &rudzio_root))?;
        let aggregator = out_dir.join("Cargo.toml");
        let deps = read_dependencies_section(&aggregator)?;
        let entry = deps.get("target_cfg_dep_via_registry").ok_or_else(|| {
            anyhow::anyhow!(
                "target_cfg_dep_via_registry missing from aggregator Cargo.toml:\n{}",
                fs::read_to_string(&aggregator).unwrap_or_default()
            )
        })?;
        anyhow::ensure!(
            entry.contains("registry = \"fthm\""),
            "target-cfg dep must keep registry = \"fthm\" in aggregator, got: {entry}\n\nfull file:\n{}",
            fs::read_to_string(&aggregator).unwrap_or_default()
        );
        Ok(())
    }

    #[rudzio::test]
    fn bridge_preserves_registry_on_all_dep_locations() -> anyhow::Result<()> {
        let rudzio_root = rudzio_source_root();
        let (_temp, out_dir) = drive_pipeline(|root| write_fixture(root, &rudzio_root))?;
        let bridge = out_dir.join("members").join("consumer").join("Cargo.toml");
        let deps = read_dependencies_section(&bridge)?;
        for dep_name in [
            "direct_dep_via_registry",
            "ws_dep_via_registry",
            "dev_dep_via_registry",
            "target_cfg_dep_via_registry",
        ] {
            let entry = deps.get(dep_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "{dep_name} missing from bridge Cargo.toml:\n{}",
                    fs::read_to_string(&bridge).unwrap_or_default()
                )
            })?;
            anyhow::ensure!(
                entry.contains("registry = \"fthm\""),
                "{dep_name} must keep registry = \"fthm\" in bridge, got: {entry}\n\nfull file:\n{}",
                fs::read_to_string(&bridge).unwrap_or_default()
            );
        }
        Ok(())
    }

    #[rudzio::test]
    fn aggregator_preserves_registry_on_rudzio_itself() -> anyhow::Result<()> {
        let (_temp, out_dir) = drive_pipeline(write_fixture_with_rudzio_on_registry)?;
        let aggregator = out_dir.join("Cargo.toml");
        let text = fs::read_to_string(&aggregator)
            .map_err(|err| anyhow::anyhow!("reading {}: {err}", aggregator.display()))?;
        // The rudzio entry sits in the aggregator's `[dependencies]`
        // table; `read_dependencies_section` covers it, but we double-
        // check by name because the bridge-vs-direct emit branches
        // pick different inline-table shapes.
        let deps = read_dependencies_section(&aggregator)?;
        let entry = deps
            .get("rudzio")
            .ok_or_else(|| anyhow::anyhow!("rudzio missing from aggregator Cargo.toml:\n{text}"))?;
        anyhow::ensure!(
            entry.contains("registry = \"fthm\""),
            "rudzio must keep registry = \"fthm\" when declared via registry, got: {entry}\n\nfull file:\n{text}",
        );
        anyhow::ensure!(
            entry.contains("version = \"=0.1.0\""),
            "rudzio must keep its version requirement next to the registry, got: {entry}\n\nfull file:\n{text}",
        );
        Ok(())
    }
}
