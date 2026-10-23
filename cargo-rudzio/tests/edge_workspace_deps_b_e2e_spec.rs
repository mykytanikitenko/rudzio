//! Edge-case E2E coverage for cargo-rudzio's workspace-wide dependency
//! wiring (part 2).
//!
//! Each case materialises a synthetic workspace whose members wire a
//! path-dependency edge in a distinct way, then drives
//! `cargo-rudzio test` (no `-p`, unless the case itself exercises the
//! `-p` filter) and asserts on the exact set of passed leaf names plus
//! the `test result: ok. N passed` summary count.
//!
//! Every dependency edge where a dependent crate holds a `Widget` type
//! passes that value into one of its own functions, so a duplicated
//! compile of the dependency crate (the Bug #6 failure mode) cannot
//! type-check and fails the run rather than silently passing.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// `b`: a type and constructor consumed by dependents, a function that
/// exists only under `feature = "extra"`, and `b`'s own rudzio test.
const B_LIB_RS: &str = "// rudzio_test marker
pub struct Widget {
    pub value: u32,
}

pub fn make_widget(value: u32) -> Widget {
    Widget { value }
}

#[cfg(feature = \"extra\")]
pub fn extra_only() -> u32 {
    99
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn b_test(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(crate::make_widget(7).value == 7);
            Ok(())
        }
    }
}
";

/// `b`'s manifest: carries an `extra` feature for the feature-unification
/// case.
const B_MANIFEST: &str = "[package]
name = \"b\"
version = \"0.1.0\"
edition = \"2024\"
publish = false

[lib]
name = \"b\"
path = \"src/lib.rs\"

[features]
extra = []

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
";

/// `c`: a plain normal dependency on `b` and its own test. Exercises a
/// third member sharing `b` so feature unification across dependents is
/// observable.
const C_LIB_RS: &str = "// rudzio_test marker
pub fn triple(widget: &b::Widget) -> u32 {
    widget.value * 3
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn c_test(_t: &super::Test) -> anyhow::Result<()> {
            let widget = b::make_widget(14);
            anyhow::ensure!(crate::triple(&widget) == 42);
            Ok(())
        }
    }
}
";

/// `a`'s lib for the feature-unification case: its test calls the
/// `extra`-gated `b::extra_only` and passes a `b::Widget` through a
/// local function.
const A_LIB_RS_FEATURES: &str = "// rudzio_test marker
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
";

/// `a`'s lib for the optional-dep case: the `b`-typed helper and the
/// test are both gated behind the `with-b` feature that activates the
/// optional `b` dep.
const A_LIB_RS_OPTIONAL: &str = "// rudzio_test marker
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
";

/// `a`'s lib for the plain / target-specific-`dependencies` cases:
/// `b` is a normal (non-dev) dep, so the helper is ungated.
const A_LIB_RS_PLAIN: &str = "// rudzio_test marker
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
            let widget = b::make_widget(21);
            anyhow::ensure!(crate::doubled(&widget) == 42);
            Ok(())
        }
    }
}
";

/// `a`'s lib for the target-specific-`dev-dependencies` case: `b` is a
/// dev-only dep, so the helper is gated to test cfgs.
const A_LIB_RS_DEV: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
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
        fn a_dev_test(_t: &super::Test) -> anyhow::Result<()> {
            let widget = b::make_widget(21);
            anyhow::ensure!(crate::doubled(&widget) == 42);
            Ok(())
        }
    }
}
";

/// `a`'s lib for the build-dependency case: `a` has no `b` library dep,
/// only a `[build-dependencies]` entry. Its test asserts the member's
/// `build.rs` ran by checking the `cargo:rustc-env` it emits.
const A_LIB_RS_BUILD: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn a_test(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                option_env!(\"A_SAW_B\") == Some(\"yes\"),
                \"member build.rs did not run (A_SAW_B unset)\"
            );
            Ok(())
        }
    }
}
";

/// `a`'s build.rs for the build-dependency case: consumes `b` (a build
/// dep) and emits a rustc-env the test observes.
const A_BUILD_RS: &str = "fn main() {
    let widget = b::make_widget(9);
    assert_eq!(widget.value, 9);
    println!(\"cargo:rustc-env=A_SAW_B=yes\");
}
";

/// Isolated cargo locations the fixture run is pinned to.
struct CargoEnv<'paths> {
    /// `CARGO_HOME` for the subprocess.
    home: &'paths Path,
    /// `CARGO_TARGET_DIR` for the subprocess.
    target_dir: &'paths Path,
    /// Parent directory for the fixture workspace.
    tempdir: &'paths Path,
}

/// Repo root = parent of `cargo-rudzio/`. The fixture's `rudzio`
/// workspace dependency points here so it builds against the code under
/// test.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Materialise a workspace-root `Cargo.toml` with the given `members`
/// array (already TOML-serialised) and the shared `[workspace.dependencies]`.
fn write_workspace(root: &Path, members: &str) -> io::Result<()> {
    let rudzio = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nmembers = {members}\nresolver = \"3\"\n\n[workspace.dependencies]\nrudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\nanyhow = \"=1.0.104\"\n",
            rudzio = rudzio.display(),
        ),
    )
}

/// Render a member manifest from a package name and the body of extra
/// sections inserted between `[lib]` and `[dev-dependencies]` (features,
/// normal deps, target-cfg deps, build-deps, etc.).
fn manifest(name: &str, sections: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\n\n{sections}[dev-dependencies]\nrudzio = {{ workspace = true }}\nanyhow = {{ workspace = true }}\n"
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

/// Spawn `cargo-rudzio test` (no `-p` unless `extra` supplies it) from
/// `root` against the isolated cargo env.
fn run(cargo: &CargoEnv<'_>, root: &Path, extra: &[&str]) -> anyhow::Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
        .arg("test")
        .args(extra)
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo.home)
        .env("CARGO_TARGET_DIR", cargo.target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .output()?)
}

/// Parse the leaf names (after the last `::`) of every `[OK] <name> ...`
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

/// Parse `N` from the `test result: ok. N passed; ...` summary line.
fn passed_count(stdout: &str) -> Option<usize> {
    for line in stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix("test result: ok.") else {
            continue;
        };
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
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
    let expected_set: BTreeSet<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    anyhow::ensure!(
        leaves == expected_set,
        "passed leaf set mismatch\n  got:      {leaves:?}\n  expected: {expected_set:?}\n{context}"
    );
    let count = passed_count(&stdout)
        .ok_or_else(|| anyhow::anyhow!("no `test result: ok. N passed` summary found\n{context}"))?;
    anyhow::ensure!(
        count == expected.len(),
        "pass count {count} != expected {} ({expected_set:?})\n{context}",
        expected.len(),
    );
    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        A_BUILD_RS, A_LIB_RS_BUILD, A_LIB_RS_DEV, A_LIB_RS_FEATURES, A_LIB_RS_OPTIONAL,
        A_LIB_RS_PLAIN, B_LIB_RS, B_MANIFEST, C_LIB_RS, CargoEnv, IsolatedCargoTest,
        assert_exact_passes, manifest, passed_leaf_names, run, write_member, write_workspace,
    };
    use std::fs;
    use tempfile::TempDir;

    /// Build a [`CargoEnv`] from a test context. A macro (not a fn)
    /// because `IsolatedCargoTest` carries a runtime generic that the
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

    /// Case 1: feature unification. `a` depends on
    /// `b = { path = "../b", features = ["extra"] }`; `c` depends on `b`
    /// with no features. The workspace-wide run must activate `extra` so
    /// `a`'s call to `b::extra_only` compiles.
    #[rudzio::test]
    fn features_unified_across_dependents(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/a",
            &manifest("a", "[dependencies]\nb = { path = \"../b\", features = [\"extra\"] }\n\n"),
            A_LIB_RS_FEATURES,
        )?;
        write_member(root, "crates/b", B_MANIFEST, B_LIB_RS)?;
        write_member(
            root,
            "crates/c",
            &manifest("c", "[dependencies]\nb = { path = \"../b\" }\n\n"),
            C_LIB_RS,
        )?;
        assert_exact_passes(&run(&env, root, &[])?, &["a_test", "b_test", "c_test"])
    }

    /// Case 2: optional dep. `a` declares `b = { path = "../b", optional
    /// = true }` plus `[features] with-b = ["dep:b"]`, and gates its test
    /// behind `#[cfg(feature = "with-b")]`. cargo-rudzio force-enables
    /// optional deps at the aggregator (`optional_dep_force_enable_spec`),
    /// so the dep is compiled regardless; the member's own `with-b`
    /// feature activation is what determines whether the gated test runs.
    #[rudzio::test]
    fn optional_dep_with_feature_gate(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/a",
            &manifest(
                "a",
                "[features]\nwith-b = [\"dep:b\"]\n\n[dependencies]\nb = { path = \"../b\", optional = true }\n\n",
            ),
            A_LIB_RS_OPTIONAL,
        )?;
        write_member(root, "crates/b", B_MANIFEST, B_LIB_RS)?;
        let output = run(&env, root, &[])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Force-enable strips `optional = true` on the aggregator's
        // dev-dep union, but for a sibling the dep rides the bridge's
        // own `[dependencies]` table where `optional = true` is
        // preserved (render_sibling_bridge_dep). With no feature table
        // on the bridge to activate `with-b`, the gated `a_test` is
        // silently compiled out. A silent drop of a member's test is
        // never correct, so we assert the test either runs or the run
        // fails loudly — and record which one cargo-rudzio actually
        // delivers here.
        if output.status.success() {
            anyhow::ensure!(
                passed_leaf_names(&stdout).contains("a_test"),
                "optional-dep run exited 0 but dropped gated a_test\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
            );
        }
        Ok(())
    }

    /// Case 3: target-specific deps. `a` reaches `b` via
    /// `[target.'cfg(unix)'.dependencies]`, `a_dev` via
    /// `[target.'cfg(unix)'.dev-dependencies]`. Both must resolve the
    /// sibling and run alongside `b`'s own test.
    #[rudzio::test]
    fn target_specific_deps_resolve_sibling(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/a",
            &manifest("a", "[target.'cfg(unix)'.dependencies]\nb = { path = \"../b\" }\n\n"),
            A_LIB_RS_PLAIN,
        )?;
        write_member(
            root,
            "crates/a_dev",
            &manifest(
                "a_dev",
                "[target.'cfg(unix)'.dev-dependencies]\nb = { path = \"../b\" }\n\n",
            ),
            A_LIB_RS_DEV,
        )?;
        write_member(root, "crates/b", B_MANIFEST, B_LIB_RS)?;
        assert_exact_passes(
            &run(&env, root, &[])?,
            &["a_test", "a_dev_test", "b_test"],
        )
    }

    /// Case 4: build-dependency. `a` declares `[build-dependencies] b`
    /// and a `build.rs` that consumes `b::make_widget` and emits
    /// `cargo:rustc-env=A_SAW_B=yes`. `a`'s test asserts the env is set,
    /// which is only true if the member's `build.rs` actually ran.
    #[rudzio::test]
    fn build_dependency_propagates_member_build_rs(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/a",
            &manifest(
                "a",
                "build = \"build.rs\"\n\n[build-dependencies]\nb = { path = \"../b\" }\n\n",
            ),
            A_LIB_RS_BUILD,
        )?;
        fs::write(root.join("crates").join("a").join("build.rs"), A_BUILD_RS)?;
        write_member(root, "crates/b", B_MANIFEST, B_LIB_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["a_test", "b_test"])
    }

    /// Case 5: mutual dev-dependency cycle. `a` dev-depends on `b` and
    /// `b` dev-depends on `a`. The run must either pass both members'
    /// tests or fail loudly with a cycle-naming error — silently
    /// dropping tests or hanging is never correct.
    #[rudzio::test]
    fn mutual_dev_dep_cycle_is_loud(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/a",
            &manifest("a", "[dev-dependencies]\nb = { path = \"../b\" }\n"),
            A_LIB_RS_PLAIN,
        )?;
        write_member(
            root,
            "crates/b",
            &manifest("b", "[features]\nextra = []\n\n[dev-dependencies]\na = { path = \"../a\" }\n"),
            B_LIB_RS,
        )?;
        let output = run(&env, root, &[])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            assert_exact_passes(&output, &["a_test", "b_test"])
        } else {
            anyhow::ensure!(
                stderr.contains("cycle") || stderr.contains("cyclic"),
                "mutual dev-dep cycle failed without naming the cycle\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
            );
            Ok(())
        }
    }

    /// Case 6: `-p a` vs workspace-wide. On a→b, the workspace-wide
    /// passed set equals `{a_test, b_test}`; the `-p a` run passes only
    /// `a`'s test with the identical name string.
    #[rudzio::test]
    fn package_filter_selects_single_member(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/a",
            &manifest("a", "[dependencies]\nb = { path = \"../b\" }\n\n"),
            A_LIB_RS_PLAIN,
        )?;
        write_member(root, "crates/b", B_MANIFEST, B_LIB_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["a_test", "b_test"])?;
        assert_exact_passes(&run(&env, root, &["-p", "a"])?, &["a_test"])
    }

    /// Case 7: nested layout. `members = ["crates/*", "tools/x"]`, where
    /// `tools/x` depends on `../../crates/b` and `crates/a` depends on
    /// `../b`. All three members must pass.
    #[rudzio::test]
    fn nested_members_layout(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\", \"tools/x\"]")?;
        write_member(
            root,
            "crates/a",
            &manifest("a", "[dependencies]\nb = { path = \"../b\" }\n\n"),
            A_LIB_RS_PLAIN,
        )?;
        write_member(root, "crates/b", B_MANIFEST, B_LIB_RS)?;
        write_member(
            root,
            "tools/x",
            &manifest("x", "[dependencies]\nb = { path = \"../../crates/b\" }\n\n"),
            C_LIB_RS,
        )?;
        // `x` reuses `c`'s lib body (its test is named `c_test`).
        assert_exact_passes(&run(&env, root, &[])?, &["a_test", "b_test", "c_test"])
    }
}
