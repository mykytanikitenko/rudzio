//! Behavioral spec: optional `[dev-dependencies]` referenced by
//! `#[path]`-included test files are force-enabled in the aggregator's
//! generated `Cargo.toml`.
//!
//! # Symptom (pre-fix)
//!
//! A member declares `fake = { version = "...", optional = true }` in
//! its `[dev-dependencies]` and a feature gate (`[features] test =
//! ["dep:fake"]`) so that under stock cargo `cargo test --features test`
//! enables it. Under cargo-rudzio the member's dev-deps are merged
//! into the aggregator's `[dependencies]` table verbatim, so the
//! aggregator emits `fake = { version = "...", optional = true }` —
//! but the aggregator has no `[features]` table to enable the
//! optional dep, so cargo never compiles `fake` into the binary, and
//! the test files (`#[path]`-included into the aggregator) fail with
//! `unresolved import fake`.
//!
//! Workaround pre-fix: hand-patch `target/rudzio-auto-runner/Cargo.toml`
//! after every regeneration to drop the `optional = true` key.
//!
//! # Behavioral assertion
//!
//! Synthesize a member with `fake_test_dep = { version = "=0.0.1",
//! optional = true }` plus `[features] test = ["dep:fake_test_dep"]`.
//! Drive `plan_from_manifest` + `write_runner`, then read the
//! generated aggregator `Cargo.toml` and assert the `fake_test_dep`
//! entry does NOT carry `optional = true`. (Force-enable is the
//! correct semantics: the aggregator's purpose is to compile every
//! referenced dep, optional-ness is meaningless when the dep is
//! actually used.)

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Workspace root of the rudzio repo (so synthetic fixtures can
/// `path = <this>` rudzio and exercise the in-tree code).
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a single-member workspace at `root` with an optional
/// dev-dep gated by a feature. The dev-dep version (`=0.0.1`) is
/// intentionally non-existent — `cargo metadata --no-deps` doesn't
/// resolve sources, so the test only exercises the dep-table emit
/// path without hitting a real registry.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
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
    fs::create_dir_all(root.join("consumer").join("tests"))?;
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
         [features]\n\
         default = [\"test\"]\n\
         test = [\"dep:fake_test_dep\"]\n\
         \n\
         [dependencies]\n\
         fake_test_dep = { version = \"=0.0.1\", optional = true }\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.104\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    fs::write(
        root.join("consumer").join("tests").join("integration.rs"),
        "use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn placeholder() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;
    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`. Prevents
/// `cargo metadata` from trying to refresh registry indexes for the
/// synthetic fixture.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard that restores `CARGO_NET_OFFLINE` to its previous
    /// value when dropped. Survives panics in `body`.
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
                // SAFETY: see module-level comment on with_cargo_offline.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see module-level comment on with_cargo_offline.
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

/// Parse a TOML file at `path` into a `BTreeMap<String, ...>` keyed
/// by dep name; values are the full inline-table contents on a
/// single line so assertions can grep for `optional = true`.
fn read_dependencies_section(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
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
])]
mod tests {
    use super::{
        PathBuf, TempDir, fs, plan_from_manifest, read_dependencies_section, rudzio_source_root,
        with_cargo_offline, write_fixture, write_runner,
    };

    /// Synthesize the fixture, drive `plan_from_manifest` +
    /// `write_runner`, and return `(temp, out_dir)` so the test
    /// body can read the generated aggregator manifest.
    fn drive_pipeline() -> anyhow::Result<(TempDir, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        Ok((temp, out_dir))
    }

    #[rudzio::test]
    fn aggregator_force_enables_optional_dev_dep() -> anyhow::Result<()> {
        let (_temp, out_dir) = drive_pipeline()?;
        let aggregator = out_dir.join("Cargo.toml");
        let deps = read_dependencies_section(&aggregator)?;
        let entry = deps.get("fake_test_dep").ok_or_else(|| {
            anyhow::anyhow!(
                "fake_test_dep missing from aggregator Cargo.toml:\n{}",
                fs::read_to_string(&aggregator).unwrap_or_default()
            )
        })?;
        anyhow::ensure!(
            !entry.contains("optional = true"),
            "aggregator must force-enable optional dev-deps (drop `optional = true`); got: {entry}\n\nfull file:\n{}",
            fs::read_to_string(&aggregator).unwrap_or_default()
        );
        // After force-enable the dep collapses to the trivial-version
        // shape (bare `"=0.0.1"` string) because there's nothing else
        // to say. Accept either the bare string or the inline-table
        // `version = "=0.0.1"` form so a future emitter style change
        // doesn't trip this assertion.
        anyhow::ensure!(
            entry.contains("\"=0.0.1\""),
            "aggregator must keep the version pin alongside force-enable; got: {entry}",
        );
        Ok(())
    }

    /// Optional dev-dep carrying a `features = [...]` list. Force-
    /// enable must strip `optional = true` while preserving the
    /// `features` array verbatim — otherwise the dep compiles
    /// without the features that the test code expects.
    #[rudzio::test]
    fn force_enable_preserves_features_on_optional_dep() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        fs::write(
            temp.path().join("Cargo.toml"),
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
        let consumer = temp.path().join("consumer");
        fs::create_dir_all(consumer.join("src"))?;
        fs::create_dir_all(consumer.join("tests"))?;
        fs::write(
            consumer.join("Cargo.toml"),
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
             [features]\n\
             default = [\"with_feat\"]\n\
             with_feat = [\"dep:fake_feat_dep\"]\n\
             \n\
             [dependencies]\n\
             fake_feat_dep = { version = \"=0.0.1\", optional = true, features = [\"alpha\", \"beta\"] }\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true }\n",
        )?;
        fs::write(
            consumer.join("src").join("lib.rs"),
            "// rudzio_test marker\n",
        )?;
        fs::write(
            consumer.join("tests").join("integration.rs"),
            "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
        )?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        let agg = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        anyhow::ensure!(
            agg.contains("fake_feat_dep"),
            "optional dep with features must appear in aggregator deps:\n{agg}",
        );
        anyhow::ensure!(
            agg.contains("\"alpha\"") && agg.contains("\"beta\""),
            "force-enable must preserve the `features` array verbatim:\n{agg}",
        );
        // Find the fake_feat_dep line + its inline table contents so
        // we can assert `optional = true` doesn't appear on THAT
        // specific dep (other parts of the aggregator manifest may
        // legitimately contain "optional" as a feature name etc.).
        let dep_line: String = agg
            .lines()
            .find(|line| line.contains("fake_feat_dep"))
            .unwrap_or("")
            .to_owned();
        anyhow::ensure!(
            !dep_line.contains("optional = true"),
            "force-enable must strip `optional = true`; got:\n{dep_line}",
        );
        Ok(())
    }

    /// Three optional dev-deps under one member. Every one of them
    /// must be force-enabled — guards against a regression that
    /// only handles the first optional dep it encounters.
    #[rudzio::test]
    fn force_enable_applies_to_every_optional_dep_in_member() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        fs::write(
            temp.path().join("Cargo.toml"),
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
        let consumer = temp.path().join("consumer");
        fs::create_dir_all(consumer.join("src"))?;
        fs::create_dir_all(consumer.join("tests"))?;
        fs::write(
            consumer.join("Cargo.toml"),
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
             [features]\n\
             default = []\n\
             all_three = [\"dep:opt_a\", \"dep:opt_b\", \"dep:opt_c\"]\n\
             \n\
             [dependencies]\n\
             opt_a = { version = \"=0.0.1\", optional = true }\n\
             opt_b = { version = \"=0.0.2\", optional = true }\n\
             opt_c = { version = \"=0.0.3\", optional = true }\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true }\n",
        )?;
        fs::write(
            consumer.join("src").join("lib.rs"),
            "// rudzio_test marker\n",
        )?;
        fs::write(
            consumer.join("tests").join("integration.rs"),
            "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
        )?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        let agg = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        for dep in ["opt_a", "opt_b", "opt_c"] {
            anyhow::ensure!(agg.contains(dep), "aggregator must list `{dep}`:\n{agg}");
            let dep_line: String = agg
                .lines()
                .find(|line| line.contains(dep))
                .unwrap_or("")
                .to_owned();
            anyhow::ensure!(
                !dep_line.contains("optional = true"),
                "`{dep}` must be force-enabled (drop `optional = true`); got:\n{dep_line}",
            );
        }
        Ok(())
    }
}
