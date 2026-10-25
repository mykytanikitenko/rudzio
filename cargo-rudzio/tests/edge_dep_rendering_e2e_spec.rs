//! Edge-case end-to-end coverage for how `cargo-rudzio` re-renders
//! dependency specs into the generated aggregator + bridge manifests.
//!
//! This spec targets the dependency-rendering boundary that produced
//! three real defect families:
//!
//! * **Bug #6** — duplicate path-dep crates: a workspace path
//!   dependency referenced in a way the sibling-skip loop does not
//!   recognise ends up compiled at two different package IDs, so every
//!   type crossing the dependency edge fails to type-check.
//! * **The git-dep bug** — a DIRECT git dependency (`git = "…"` +
//!   `rev = "…"`) used to fall through to the version emitter and be
//!   rewritten as `version = ""`, which cargo rejects.
//! * **The `[workspace.dependencies]` `package` rename bug** — a
//!   workspace-level rename (`alias = { package = "real", … }`) whose
//!   concrete package name was not resolved back through the sibling
//!   matcher.
//!
//! Each case drives a real `cargo-rudzio test` run (exit 0 + exact
//! passed-test-name set) and, where the case is about what gets
//! *rendered*, additionally reads the generated manifest back off disk
//! and asserts on its content. Type-flow across sibling edges (`Widget`
//! passed from `b` into a dependent) makes a silently duplicated
//! sibling compile fail instead of pass, so Bug #6 cannot slip through
//! as a false green.
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Context as _;
use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// The `.cargo/config.toml` the operator ships: a repo-relative build-dir.
const DOT_CARGO_CONFIG: &str = "[build]\nbuild-dir = \"tmp/build-dir\"\n";

/// The standard dev-dep + lints tail for a bridged member.
const BRIDGED_TAIL: &str = "[dev-dependencies]\nrudzio = { workspace = true }\nanyhow = { workspace = true }\n\n[lints]\nworkspace = true\n";

/// Isolated cargo locations the fixture run is pinned to.
struct CargoEnv<'paths> {
    /// `CARGO_HOME` for the subprocess.
    home: &'paths Path,
    /// `CARGO_TARGET_DIR` for the subprocess.
    target_dir: &'paths Path,
    /// Parent directory for the fixture workspace.
    tempdir: &'paths Path,
}

/// Repo root = parent of `cargo-rudzio/`. Fixtures point their `rudzio`
/// workspace dependency here so they exercise the in-tree code.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Render a workspace-root manifest. `members` is a pre-formatted TOML
/// array (`["crates/*"]` or `["crates/*", "tools/deep/x"]`); `extra_ws`
/// is appended verbatim inside `[workspace.dependencies]`. Operator
/// shape: `crates/*`, resolver 2, `[workspace.lints.rust]` denying
/// warnings + unsafe code with `cfg(rudzio_test)` declared, panic-abort
/// profiles, and edition inherited from `[workspace.package]`.
fn workspace_manifest(rudzio: &Path, members: &str, extra_ws: &str) -> String {
    format!(
        "[workspace]\nmembers = {members}\nresolver = \"2\"\n\n\
         [workspace.dependencies]\n\
         rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n\
         anyhow = \"=1.0.104\"\n\
         {extra_ws}\
         [workspace.package]\nedition = \"2024\"\n\n\
         [workspace.lints.rust]\nwarnings = \"deny\"\nunsafe_code = \"deny\"\n\
         unexpected_cfgs = {{ level = \"warn\", check-cfg = ['cfg(rudzio_test)'] }}\n\n\
         [profile.dev]\npanic = \"abort\"\n\n\
         [profile.release]\npanic = \"abort\"\n",
        rudzio = rudzio.display(),
    )
}

/// Materialise the workspace root + `.cargo/config.toml`.
fn write_workspace(root: &Path, members: &str, extra_ws: &str) -> io::Result<()> {
    let rudzio = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    fs::write(
        root.join("Cargo.toml"),
        workspace_manifest(rudzio, members, extra_ws),
    )?;
    fs::create_dir_all(root.join(".cargo"))?;
    fs::write(root.join(".cargo").join("config.toml"), DOT_CARGO_CONFIG)?;
    Ok(())
}

/// Render a member manifest: `[package]` + `[lib]`, then `sections`
/// verbatim (dependencies / features / target-cfg tables / dev-deps /
/// lints). The caller supplies the dev-dep + lints tail so a member
/// needing custom dev-deps can provide them inline.
fn lib_manifest(name: &str, sections: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n\
         [lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\n\n{sections}"
    )
}

/// Write a member crate at repo-relative path `rel` (e.g. `crates/a`).
fn write_member(root: &Path, rel: &str, manifest: &str, lib_rs: &str) -> io::Result<()> {
    let dir = root.join(rel);
    fs::create_dir_all(dir.join("src"))?;
    fs::write(dir.join("Cargo.toml"), manifest)?;
    fs::write(dir.join("src").join("lib.rs"), lib_rs)?;
    Ok(())
}

/// A sibling lib exposing a `Widget` type + constructor, an `extra`
/// feature, and one rudzio test named `test_name`. The `Widget` type is
/// the load-bearing type that a duplicated compile cannot reconcile.
fn widget_lib(test_name: &str) -> String {
    format!(
        "// rudzio_test marker
pub struct Widget {{
    pub value: u32,
}}

pub fn make_widget(value: u32) -> Widget {{
    Widget {{ value }}
}}

#[cfg(feature = \"extra\")]
pub fn extra_only() -> u32 {{
    99
}}

#[cfg(any(test, rudzio_test))]
mod tests {{
    use rudzio::common::context::{{Suite, Test}};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {{
        #[rudzio::test]
        fn {test_name}(_t: &super::Test) -> anyhow::Result<()> {{
            anyhow::ensure!(crate::make_widget(7).value == 7);
            Ok(())
        }}
    }}
}}
"
    )
}

/// A sibling manifest exposing an `extra` feature + the bridged tail.
fn widget_manifest(name: &str) -> String {
    lib_manifest(name, &format!("[features]\nextra = []\n\n{BRIDGED_TAIL}"))
}

/// A dependent lib whose test consumes `<dep>::Widget` (type-flow) and
/// runs `test_name`. `<dep>` is a NORMAL dependency, so the helper is
/// ungated.
fn consume_lib(test_name: &str, dep: &str) -> String {
    format!(
        "// rudzio_test marker
pub fn doubled(widget: &{dep}::Widget) -> u32 {{
    widget.value * 2
}}

#[cfg(any(test, rudzio_test))]
mod tests {{
    use rudzio::common::context::{{Suite, Test}};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {{
        #[rudzio::test]
        fn {test_name}(_t: &super::Test) -> anyhow::Result<()> {{
            let widget = {dep}::make_widget(21);
            anyhow::ensure!(crate::doubled(&widget) == 42);
            Ok(())
        }}
    }}
}}
"
    )
}

/// A dependent lib whose test consumes `<dep>::Widget` where `<dep>` is
/// a DEV-only dependency, so the helper is gated to test cfgs.
fn consume_lib_dev(test_name: &str, dep: &str) -> String {
    format!(
        "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
pub fn doubled(widget: &{dep}::Widget) -> u32 {{
    widget.value * 2
}}

#[cfg(any(test, rudzio_test))]
mod tests {{
    use rudzio::common::context::{{Suite, Test}};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {{
        #[rudzio::test]
        fn {test_name}(_t: &super::Test) -> anyhow::Result<()> {{
            let widget = {dep}::make_widget(21);
            anyhow::ensure!(crate::doubled(&widget) == 42);
            Ok(())
        }}
    }}
}}
"
    )
}

/// A lib carrying only a self-contained rudzio test named `test_name`
/// (no inter-crate type flow).
fn plain_test_lib(test_name: &str) -> String {
    format!(
        "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {{
    use rudzio::common::context::{{Suite, Test}};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {{
        #[rudzio::test]
        fn {test_name}(_t: &super::Test) -> anyhow::Result<()> {{
            Ok(())
        }}
    }}
}}
"
    )
}

/// Spawn `cargo-rudzio test` (via the fresh binary) from `root` against
/// the isolated cargo env. `offline` controls `CARGO_NET_OFFLINE`; git
/// fixtures need it unset so a local `file://` fetch is permitted.
fn run(cargo: &CargoEnv<'_>, root: &Path, extra: &[&str], offline: bool) -> anyhow::Result<Output> {
    Ok(
        Command::new(cargo_rudzio_test_helpers::assert_fresh_binary(env!(
            "CARGO_BIN_EXE_cargo-rudzio"
        ))?)
        .arg("test")
        .args(extra)
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo.home)
        .env("CARGO_TARGET_DIR", cargo.target_dir)
        .env("CARGO_NET_OFFLINE", if offline { "true" } else { "false" })
        .output()?,
    )
}

/// Parse the leaf names (after the last `::`) of every `[OK] <name> …`
/// line, excluding suite `setup` / `teardown` hooks.
fn passed_leaf_names(stdout: &str) -> BTreeSet<String> {
    let mut leaves = BTreeSet::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("[OK] ") else {
            continue;
        };
        let name = rest.split_whitespace().next().unwrap_or("");
        let leaf = name.rsplit("::").next().unwrap_or(name);
        if leaf == "setup" || leaf == "teardown" {
            continue;
        }
        let _inserted: bool = leaves.insert(leaf.to_owned());
    }
    leaves
}

/// Parse `N` from the `test result: ok. N passed; …` summary line.
fn passed_count(stdout: &str) -> Option<usize> {
    for line in stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix("test result: ok.") else {
            continue;
        };
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
    }
    None
}

/// Assert the run exited 0, the passed leaf set equals `expected`
/// exactly, and the summary `N passed` equals `expected.len()`. On any
/// mismatch, print full stdout and stderr.
fn assert_exact_passes(output: &Output, expected: &[&str]) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    anyhow::ensure!(
        output.status.success(),
        "run exited {:?}\n{context}",
        output.status.code(),
    );
    let leaves = passed_leaf_names(&stdout);
    let expected_set: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();
    anyhow::ensure!(
        leaves == expected_set,
        "passed leaf set mismatch\n  got:      {leaves:?}\n  expected: {expected_set:?}\n{context}"
    );
    let count = passed_count(&stdout).ok_or_else(|| {
        anyhow::anyhow!("no `test result: ok. N passed` summary found\n{context}")
    })?;
    anyhow::ensure!(
        count == expected.len(),
        "pass count {count} != expected {} ({expected_set:?})\n{context}",
        expected.len(),
    );
    Ok(())
}

/// Locate the single `rudzio-auto-runner-*` directory under the per-test
/// cargo target dir. `write_runner` always emits exactly one.
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

/// Parse a generated manifest's dependency tables into a map keyed by
/// dep name → full inline-table contents (one line). Tracks top-level
/// `[dependencies]`/`[dev-dependencies]` and every
/// `[target.'cfg(...)'.dependencies]`/`.dev-dependencies]` table.
fn read_dependencies_section(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]"
                || trimmed == "[dev-dependencies]"
                || trimmed.ends_with(".dependencies]")
                || trimmed.ends_with(".dev-dependencies]");
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

/// Read the generated aggregator manifest's dependency map.
fn aggregator_deps(cargo_target_dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let agg = find_aggregator_dir(cargo_target_dir)?.join("Cargo.toml");
    read_dependencies_section(&agg)
}

/// Read a generated bridge manifest's dependency map.
fn bridge_deps(cargo_target_dir: &Path, member: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let bridge = find_aggregator_dir(cargo_target_dir)?
        .join("members")
        .join(member)
        .join("Cargo.toml");
    read_dependencies_section(&bridge)
}

/// Create a tiny `gitdep` lib crate inside a fresh git repo at
/// `root/git-dep`, commit it, and return `(file:// url, rev)`.
fn make_git_dep_repo(root: &Path) -> anyhow::Result<(String, String)> {
    let repo = root.join("git-dep");
    fs::create_dir_all(repo.join("src"))?;
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"gitdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )?;
    fs::write(
        repo.join("src").join("lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )?;
    git(&repo, &["init", "-q"])?;
    git(&repo, &["add", "-A"])?;
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    )?;
    let rev = git_output(&repo, &["rev-parse", "HEAD"])?;
    Ok((format!("file://{}", repo.display()), rev))
}

/// Run a `git` command in `repo`, failing loudly on non-zero exit.
fn git(repo: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").current_dir(repo).args(args).output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(())
}

/// Run a `git` command in `repo` and return its trimmed stdout.
fn git_output(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").current_dir(repo).args(args).output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        BRIDGED_TAIL, CargoEnv, IsolatedCargoTest, TempDir, aggregator_deps, assert_exact_passes,
        bridge_deps, consume_lib, consume_lib_dev, lib_manifest, make_git_dep_repo, plain_test_lib,
        run, widget_lib, widget_manifest, write_member, write_workspace,
    };

    /// Build a [`CargoEnv`] from a test context. A macro (not a fn)
    /// because [`IsolatedCargoTest`] carries a runtime generic that the
    /// `#[rudzio::test]` rewrite elides in test-body signatures.
    macro_rules! cargo_env {
        ($ctx:expr) => {
            CargoEnv {
                tempdir: $ctx.suite_tempdir(),
                home: $ctx.cargo_home(),
                target_dir: $ctx.cargo_target_dir(),
            }
        };
    }

    /// Case 1: a registry dev-dep with `default-features = false` and
    /// `features` must be re-rendered verbatim (not silently dropped,
    /// which would change the build's feature set). `libc` is used as
    /// the registry crate because it is already in the pre-warmed cargo
    /// home (transitive dep of `tokio`'s `full` features).
    #[rudzio::test]
    fn registry_dev_dep_default_features_false_is_preserved(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]", "")?;
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                &format!(
                    "[dependencies]\nlibc = {{ version = \"=0.2.191\", default-features = false, features = [\"extra_traits\"] }}\n\n{BRIDGED_TAIL}"
                ),
            ),
            &plain_test_lib("a_ok"),
        )?;
        assert_exact_passes(&run(&env, root, &[], true)?, &["a_ok"])?;
        let deps = aggregator_deps(ctx.cargo_target_dir())?;
        let entry = deps
            .get("libc")
            .ok_or_else(|| anyhow::anyhow!("libc missing from aggregator deps: {deps:?}"))?;
        anyhow::ensure!(
            entry.contains("default-features = false"),
            "libc must keep `default-features = false`, got: {entry}"
        );
        anyhow::ensure!(
            entry.contains("extra_traits"),
            "libc must keep its `features`, got: {entry}"
        );
        Ok(())
    }

    /// Case 2: a registry dev-dep with an exact pin plus target-specific
    /// features (`[target.'cfg(unix)'.dev-dependencies]`) must preserve
    /// both the pin and the features.
    #[rudzio::test]
    fn registry_dev_dep_target_specific_features_preserved(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]", "")?;
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                &format!(
                    "[target.'cfg(unix)'.dev-dependencies]\nlibc = {{ version = \"=0.2.191\", features = [\"extra_traits\"] }}\n\n{BRIDGED_TAIL}"
                ),
            ),
            &plain_test_lib("a_ok"),
        )?;
        assert_exact_passes(&run(&env, root, &[], true)?, &["a_ok"])?;
        let deps = aggregator_deps(ctx.cargo_target_dir())?;
        let entry = deps
            .get("libc")
            .ok_or_else(|| anyhow::anyhow!("libc missing from aggregator deps: {deps:?}"))?;
        anyhow::ensure!(
            entry.contains("=0.2.191"),
            "libc must keep its exact pin, got: {entry}"
        );
        anyhow::ensure!(
            entry.contains("extra_traits"),
            "libc must keep its `features`, got: {entry}"
        );
        Ok(())
    }

    /// Case 3: a sibling path dep declared in the publishable form
    /// (`path` AND `version`) must still compile the sibling exactly
    /// once — the `version` key must not cause cargo-rudzio to misroute
    /// the dep into a version-only (duplicated) resolution.
    #[rudzio::test]
    fn sibling_path_dep_with_version_publishable_form(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]", "")?;
        write_member(
            root,
            "crates/b",
            &widget_manifest("b"),
            &widget_lib("b_test"),
        )?;
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                &format!(
                    "[dependencies]\nb = {{ path = \"../b\", version = \"0.1.0\" }}\n\n{BRIDGED_TAIL}"
                ),
            ),
            &consume_lib("a_test", "b"),
        )?;
        assert_exact_passes(&run(&env, root, &[], true)?, &["a_test", "b_test"])
    }

    /// Case 4: a workspace dependency declared with `path` relative to
    /// the workspace root, inherited by a member at a different depth
    /// (`tools/deep/x`), must resolve the sibling (not a depth-relative
    /// path).
    #[rudzio::test]
    fn workspace_inherited_path_dep_at_different_depth(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(
            root,
            "[\"crates/*\", \"tools/deep/x\"]",
            "b = { path = \"crates/b\" }\n",
        )?;
        write_member(
            root,
            "crates/b",
            &widget_manifest("b"),
            &widget_lib("b_test"),
        )?;
        write_member(
            root,
            "tools/deep/x",
            &lib_manifest(
                "x",
                &format!("[dependencies]\nb = {{ workspace = true }}\n\n{BRIDGED_TAIL}"),
            ),
            &consume_lib("x_test", "b"),
        )?;
        assert_exact_passes(&run(&env, root, &[], true)?, &["b_test", "x_test"])
    }

    /// Case 5: the same sibling declared in both `[dependencies]` and
    /// `[dev-dependencies]` with different feature sets (cargo unifies
    /// them) must not lose the extra feature.
    #[rudzio::test]
    fn same_dep_in_dependencies_and_dev_dependencies_unifies(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]", "")?;
        write_member(
            root,
            "crates/b",
            &widget_manifest("b"),
            &widget_lib("b_test"),
        )?;
        // `a` declares `b` in both tables; the normal declaration
        // carries `features = ["extra"]`. The test below calls
        // `b::extra_only()` which only exists when `extra` is unified.
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                "[dependencies]\n\
                 b = { path = \"../b\", features = [\"extra\"] }\n\n\
                 [dev-dependencies]\n\
                 b = { path = \"../b\" }\n\
                 rudzio = { workspace = true }\n\
                 anyhow = { workspace = true }\n\n\
                 [lints]\nworkspace = true\n",
            ),
            "// rudzio_test marker
pub fn doubled(widget: &b::Widget) -> u32 {
    widget.value * 2
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn a_test(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(b::extra_only() == 99);
            let widget = b::make_widget(21);
            anyhow::ensure!(crate::doubled(&widget) == 42);
            Ok(())
        }
    }
}
",
        )?;
        assert_exact_passes(&run(&env, root, &[], true)?, &["a_test", "b_test"])
    }

    /// Case 6: an optional sibling dep with `default-features = false`,
    /// enabled via `dep:b`, must neither duplicate `b` nor crash the
    /// run; the gated `a` test does not run (matches `cargo test`
    /// without `--features`).
    #[rudzio::test]
    fn optional_sibling_dep_default_features_false(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]", "")?;
        write_member(
            root,
            "crates/b",
            &widget_manifest("b"),
            &widget_lib("b_test"),
        )?;
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                &format!(
                    "[features]\nwith-b = [\"dep:b\"]\n\n[dependencies]\nb = {{ path = \"../b\", optional = true, default-features = false }}\n\n{BRIDGED_TAIL}"
                ),
            ),
            // Gated test: under `cargo rudzio test` (no `--features`)
            // the `with-b` feature is not enabled, so `a_test` is
            // compiled out and only `b_test` runs.
            "// rudzio_test marker
#[cfg(feature = \"with-b\")]
pub fn doubled(widget: &b::Widget) -> u32 {
    widget.value * 2
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[cfg(feature = \"with-b\")]
        #[rudzio::test]
        fn a_test(_t: &super::Test) -> anyhow::Result<()> {
            let widget = b::make_widget(21);
            anyhow::ensure!(crate::doubled(&widget) == 42);
            Ok(())
        }
    }
}
",
        )?;
        assert_exact_passes(&run(&env, root, &[], true)?, &["b_test"])
    }

    /// Case 7: one member dev-depends on the sibling via
    /// `workspace = true` while another depends via direct path; the
    /// sibling must compile exactly once (Widget type-flow across both).
    #[rudzio::test]
    fn workspace_inherited_and_direct_path_dep_compile_once(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]", "b = { path = \"crates/b\" }\n")?;
        write_member(
            root,
            "crates/b",
            &widget_manifest("b"),
            &widget_lib("b_test"),
        )?;
        // `a` dev-depends on `b` via workspace = true.
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                "[dev-dependencies]\n\
                 b = { workspace = true }\n\
                 rudzio = { workspace = true }\n\
                 anyhow = { workspace = true }\n\n\
                 [lints]\nworkspace = true\n",
            ),
            &consume_lib_dev("a_test", "b"),
        )?;
        // `c` depends on `b` via direct path.
        write_member(
            root,
            "crates/c",
            &lib_manifest(
                "c",
                &format!("[dependencies]\nb = {{ path = \"../b\" }}\n\n{BRIDGED_TAIL}"),
            ),
            &consume_lib("c_test", "b"),
        )?;
        assert_exact_passes(
            &run(&env, root, &[], true)?,
            &["a_test", "b_test", "c_test"],
        )
    }

    /// Case 8: a DIRECT git dependency on a NON-rudzio crate must be
    /// preserved (git + rev), never dropped or rewritten to an empty
    /// `version = ""`.
    #[rudzio::test]
    fn non_rudzio_git_dep_is_preserved(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        let (git_url, rev) = make_git_dep_repo(root)?;
        write_workspace(root, "[\"crates/*\"]", "")?;
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                &format!(
                    "[dev-dependencies]\ngitdep = {{ git = \"{git_url}\", rev = \"{rev}\" }}\nrudzio = {{ workspace = true }}\nanyhow = {{ workspace = true }}\n\n[lints]\nworkspace = true\n"
                ),
            ),
            &plain_test_lib("a_ok"),
        )?;
        assert_exact_passes(&run(&env, root, &[], false)?, &["a_ok"])?;
        let deps = aggregator_deps(ctx.cargo_target_dir())?;
        let entry = deps
            .get("gitdep")
            .ok_or_else(|| anyhow::anyhow!("gitdep missing from aggregator deps: {deps:?}"))?;
        anyhow::ensure!(
            entry.contains(&format!("git = \"{git_url}\"")),
            "gitdep must keep its git URL, got: {entry}"
        );
        anyhow::ensure!(
            entry.contains(&format!("rev = \"{rev}\"")),
            "gitdep must keep its rev pin, got: {entry}"
        );
        anyhow::ensure!(
            !entry.contains("version = \"\""),
            "gitdep must NOT be rewritten to an empty version, got: {entry}"
        );
        Ok(())
    }

    /// Case 9: a workspace-level `package` rename
    /// (`bee = { package = "b", path = "crates/b" }`) inherited via
    /// `bee = { workspace = true }` must resolve the real sibling and
    /// re-render the `package` key in the bridge manifest.
    #[rudzio::test]
    fn workspace_dependencies_package_rename_rendered(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(
            root,
            "[\"crates/*\"]",
            "bee = { package = \"b\", path = \"crates/b\" }\n",
        )?;
        write_member(
            root,
            "crates/b",
            &widget_manifest("b"),
            &widget_lib("b_test"),
        )?;
        write_member(
            root,
            "crates/a",
            &lib_manifest(
                "a",
                &format!("[dependencies]\nbee = {{ workspace = true }}\n\n{BRIDGED_TAIL}"),
            ),
            &consume_lib("a_test", "bee"),
        )?;
        assert_exact_passes(&run(&env, root, &[], true)?, &["a_test", "b_test"])?;
        let deps = bridge_deps(ctx.cargo_target_dir(), "a")?;
        let entry = deps
            .get("bee")
            .ok_or_else(|| anyhow::anyhow!("bee missing from bridge deps: {deps:?}"))?;
        anyhow::ensure!(
            entry.contains("package ="),
            "renamed ws sibling must re-emit a `package` key, got: {entry}"
        );
        Ok(())
    }
}
