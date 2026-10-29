//! End-to-end reproducer for how `cargo-rudzio` renders the
//! `[dependencies]` table of a self-aggregating `#[rudzio::main]`
//! standalone-bin crate.
//!
//! A standalone `#[rudzio::main]` bin is emitted as its own bin-only
//! crate that links the REAL dependency graph (not the test-only
//! bridges). Two defects in that rendering broke plain `just test`:
//!
//! * **Dev-only leak** — the standalone bin's `[dependencies]` was built
//!   from the member's merged `[dependencies]` ∪ `[dev-dependencies]`
//!   with no section filter, so a DEV-only sibling dep leaked into the
//!   bin's regular `[dependencies]` (plain `cargo build --bin` links no
//!   dev-deps, so the standalone bin must not either).
//! * **Lib-ident key** — a non-renamed bridged sibling was keyed by its
//!   underscored lib identifier (`foo_helpers`) while the renderer
//!   emitted only `path = …` (no `package = …`), so cargo looked for a
//!   package literally named `foo_helpers` and failed resolution.
//!
//! The operator repro: `fixtures` dev-depends on the workspace member
//! `cargo-rudzio-test-helpers` (package name hyphenated, lib name
//! `cargo_rudzio_test_helpers`), and `cargo rudzio test` errored with
//! `no matching package named cargo_rudzio_test_helpers found`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context as _;
use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/`. Fixtures point their `rudzio`
/// workspace dependency here so they exercise the in-tree code.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Workspace root manifest (minimal: no panic/profile overrides, no
/// deny-warnings) so the standalone bin compiles without the
/// panic-abort-propagation or clippy-deny interactions.
fn workspace_manifest(rudzio: &Path, members: &str) -> String {
    format!(
        "[workspace]\nmembers = {members}\nresolver = \"2\"\n\n\
         [workspace.dependencies]\n\
         rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n\
         anyhow = \"=1.0.104\"\n",
        rudzio = rudzio.display(),
    )
}

/// Materialise the workspace root manifest.
fn write_workspace(root: &Path, members: &str) -> io::Result<()> {
    let rudzio = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    fs::write(root.join("Cargo.toml"), workspace_manifest(rudzio, members))
}

/// A bridged sibling whose package name (`foo-helpers`) differs from its
/// lib identifier (`foo_helpers`) — the shape that trips the lib-ident
/// key defect.
const FOO_HELPERS_MANIFEST: &str = "[package]\n\
     name = \"foo-helpers\"\n\
     version = \"0.1.0\"\n\
     edition = \"2024\"\n\
     publish = false\n\
     \n\
     [lib]\n\
     name = \"foo_helpers\"\n\
     path = \"src/lib.rs\"\n\
     \n\
     [dev-dependencies]\n\
     rudzio = { workspace = true }\n\
     anyhow = { workspace = true }\n";

const FOO_HELPERS_LIB: &str = "//! sibling lib whose crate name != package name
pub fn answer() -> u32 {
    42
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn foo_helpers_ok(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(crate::answer() == 42);
            Ok(())
        }
    }
}
";

/// A self-aggregating `#[rudzio::main]` bin source that consumes the
/// sibling's lib (`foo_helpers::answer()`), proving the dep resolves.
const RUNNER_BIN_USES_SIBLING: &str = "use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([(runtime = Multithread::new, suite = Suite, test = Test)])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn a_uses_foo_helpers(_t: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(foo_helpers::answer() == 42);
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
";

/// A self-contained `#[rudzio::main]` bin source that does not reference
/// the sibling (so a dev-only sibling dep can be safely dropped).
const RUNNER_BIN_SELF_CONTAINED: &str = "use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([(runtime = Multithread::new, suite = Suite, test = Test)])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn a_self_contained(_t: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
";

/// Member `a` manifest. `foo_helpers_in_dev` places `foo-helpers` in
/// `[dev-dependencies]` (DEV-only) vs `[dependencies]` (REGULAR).
/// `anyhow` is ALWAYS a regular dep because the standalone bin source's
/// `#[rudzio::test]` bodies return `anyhow::Result` and plain
/// `cargo build --bin` links only regular deps.
fn member_a_manifest(foo_helpers_in_dev: bool) -> String {
    let foo_helpers_line = "foo-helpers = { path = \"../foo-helpers\" }\n";
    let (dependencies, dev_dependencies) = if foo_helpers_in_dev {
        (
            "anyhow = { workspace = true }\n".to_owned(),
            format!("{foo_helpers_line}rudzio = {{ workspace = true }}\n"),
        )
    } else {
        (
            format!("anyhow = {{ workspace = true }}\n{foo_helpers_line}"),
            "rudzio = { workspace = true }\n".to_owned(),
        )
    };
    format!(
        "[package]\n\
         name = \"a\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [[bin]]\n\
         name = \"runner\"\n\
         path = \"src/bin/runner.rs\"\n\
         test = false\n\
         \n\
         [dependencies]\n\
         {dependencies}\
         \n\
         [dev-dependencies]\n\
         {dev_dependencies}"
    )
}

/// Write the `foo-helpers` sibling crate at `root/foo-helpers`.
fn write_foo_helpers(root: &Path) -> io::Result<()> {
    let dir = root.join("foo-helpers");
    fs::create_dir_all(dir.join("src"))?;
    fs::write(dir.join("Cargo.toml"), FOO_HELPERS_MANIFEST)?;
    fs::write(dir.join("src").join("lib.rs"), FOO_HELPERS_LIB)?;
    Ok(())
}

/// Write member `a` at `root/a` with a `src/bin/runner.rs` standalone bin.
fn write_member_a(root: &Path, foo_helpers_in_dev: bool, bin_source: &str) -> io::Result<()> {
    let dir = root.join("a");
    fs::create_dir_all(dir.join("src").join("bin"))?;
    fs::write(
        dir.join("Cargo.toml"),
        member_a_manifest(foo_helpers_in_dev),
    )?;
    fs::write(dir.join("src").join("bin").join("runner.rs"), bin_source)?;
    Ok(())
}

/// Spawn `cargo rudzio test` (via the fresh binary) from `root` against
/// the isolated cargo env.
fn run_cargo_rudzio(root: &Path, cargo_home: &Path, cargo_target_dir: &Path) -> io::Result<Output> {
    Command::new(
        cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .map_err(io::Error::other)?,
    )
    .arg("test")
    .current_dir(root)
    .env(
        "CARGO",
        env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
    )
    .env("CARGO_HOME", cargo_home)
    .env("CARGO_TARGET_DIR", cargo_target_dir)
    .env("CARGO_NET_OFFLINE", "true")
    .env("NO_COLOR", "1")
    .output()
}

/// Locate the single `rudzio-auto-runner-*` directory under the per-test
/// cargo target dir.
fn find_aggregator_dir(cargo_target_dir: &Path) -> anyhow::Result<PathBuf> {
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(cargo_target_dir)
        .with_context(|| format!("reading target dir {}", cargo_target_dir.display()))?
    {
        let dir_entry = entry?;
        let file_name = dir_entry.file_name();
        let dir_name = file_name.to_string_lossy();
        if dir_name.starts_with("rudzio-auto-runner") && dir_entry.file_type()?.is_dir() {
            matches.push(dir_entry.path());
        }
    }
    anyhow::ensure!(
        matches.len() == 1,
        "expected exactly one `rudzio-auto-runner-*` dir under {}, got {}: {:?}",
        cargo_target_dir.display(),
        matches.len(),
        matches,
    );
    matches
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected one `rudzio-auto-runner-*` dir"))
}

/// Parse a manifest's top-level `[dependencies]` (and target-cfg
/// `.dependencies`) into a map keyed by dep name → inline-table body.
fn read_dependencies_section(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]" || trimmed.ends_with(".dependencies]");
            continue;
        }
        if !in_deps {
            continue;
        }
        if let Some((name, rest)) = trimmed.split_once('=') {
            let _previous = out.insert(name.trim().to_owned(), rest.trim().to_owned());
        }
    }
    Ok(out)
}

/// Read the generated standalone-bin crate's `[dependencies]` map for
/// `member`/`bin` (dir `<agg>/members/<member>__standalone__<bin>/`).
fn standalone_bin_deps(
    cargo_target_dir: &Path,
    member: &str,
    bin: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let dir = find_aggregator_dir(cargo_target_dir)?
        .join("members")
        .join(format!("{member}__standalone__{bin}"));
    read_dependencies_section(&dir.join("Cargo.toml"))
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, RUNNER_BIN_SELF_CONTAINED, RUNNER_BIN_USES_SIBLING, TempDir,
        run_cargo_rudzio, standalone_bin_deps, write_foo_helpers, write_member_a, write_workspace,
    };

    /// Defect 1: a DEV-only sibling dep must NOT leak into the
    /// standalone bin's `[dependencies]`.
    #[rudzio::test]
    fn standalone_bin_drops_dev_only_sibling_dep(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace(root, "[\"foo-helpers\", \"a\"]")?;
        write_foo_helpers(root)?;
        write_member_a(root, true, RUNNER_BIN_SELF_CONTAINED)?;

        let out = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        anyhow::ensure!(
            out.status.success(),
            "cargo rudzio test failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let deps = standalone_bin_deps(ctx.cargo_target_dir(), "a", "runner")?;
        anyhow::ensure!(
            !deps.contains_key("foo-helpers") && !deps.contains_key("foo_helpers"),
            "dev-only sibling must not appear in standalone bin deps, got: {deps:?}",
        );
        Ok(())
    }

    /// Defect 2: a REGULAR sibling dep must be keyed by its package
    /// name (`foo-helpers`), never its underscored lib ident
    /// (`foo_helpers`), so cargo resolves the package at its path.
    #[rudzio::test]
    fn standalone_bin_keys_regular_sibling_dep_by_package_name(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace(root, "[\"foo-helpers\", \"a\"]")?;
        write_foo_helpers(root)?;
        write_member_a(root, false, RUNNER_BIN_USES_SIBLING)?;

        let out = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        anyhow::ensure!(
            out.status.success(),
            "cargo rudzio test failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let deps = standalone_bin_deps(ctx.cargo_target_dir(), "a", "runner")?;
        anyhow::ensure!(
            deps.contains_key("foo-helpers"),
            "regular sibling must be keyed by package name `foo-helpers`, got: {deps:?}",
        );
        anyhow::ensure!(
            !deps.contains_key("foo_helpers"),
            "regular sibling must NOT be keyed by lib ident `foo_helpers`, got: {deps:?}",
        );
        let entry = deps
            .get("foo-helpers")
            .ok_or_else(|| anyhow::anyhow!("foo-helpers missing from standalone bin deps"))?;
        anyhow::ensure!(
            entry.contains("path"),
            "foo-helpers entry must carry a `path`, got: {entry}",
        );
        anyhow::ensure!(
            !entry.contains("package ="),
            "non-renamed foo-helpers entry must NOT carry a `package` key, got: {entry}",
        );
        Ok(())
    }
}
