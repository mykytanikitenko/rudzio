//! Edge-case end-to-end spec for workspace-wide dependency wiring in
//! the cargo-rudzio aggregator.
//!
//! Every fixture is a synthetic workspace under `crates/*`. Each member
//! that carries a rudzio test defines its own `Widget` type plus a
//! `consume_<dep>` function whose signature names a dependency's
//! `Widget`. A member's test constructs the dependency's `Widget` via
//! `make_widget` and hands it to its own `consume_<dep>`, so a
//! dependency crate compiled at two different package IDs (Bug #6)
//! fails to type-check instead of silently passing.
//!
//! Each case runs `cargo rudzio test` from the workspace root with no
//! `-p`, requires exit 0, and asserts that the runner reported exactly
//! the expected set of passed leaf names (and the matching `N passed`
//! count).
#![cfg(unix)]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// How a member's manifest references an intra-workspace sibling.
#[derive(Clone, Copy)]
enum DepKind {
    /// `extern = { path = "../<package>" }`.
    Direct,
    /// `extern = { package = "<package>", path = "../<package>" }`.
    Renamed,
    /// `extern = { workspace = true }`.
    Workspace,
}

/// One dependency of a fixture member.
struct Dep {
    /// The extern name the member's source imports, e.g. `b`, `b1`, `bee`.
    extern_name: String,
    /// How the member's manifest writes the dependency.
    kind: DepKind,
    /// The real package name the dependency resolves to, e.g. `b`.
    package: String,
}

/// A fixture workspace member.
struct Member {
    /// Whether this member carries a rudzio test (bridged) or a plain lib.
    bridged: bool,
    /// Intra-workspace dependencies in declaration order.
    deps: Vec<Dep>,
    /// Package name; also the `crates/<name>` directory.
    name: String,
}

/// An extra `[workspace.dependencies]` entry in the root manifest.
struct WorkspaceDep {
    /// The key name, e.g. `b` or `bee`.
    key: String,
    /// The package the entry names, e.g. `b`.
    package: String,
}

/// Isolated cargo locations the fixture run is pinned to.
struct CargoEnv<'paths> {
    /// `CARGO_HOME` for the subprocess.
    home: &'paths Path,
    /// `CARGO_TARGET_DIR` for the subprocess.
    target_dir: &'paths Path,
    /// Parent directory for the fixture workspace.
    tempdir: &'paths Path,
}

/// Build a bridged member with the given intra-workspace deps.
fn bridged(name: &str, deps: Vec<Dep>) -> Member {
    Member {
        name: name.to_owned(),
        deps,
        bridged: true,
    }
}

/// Build a plain (non-bridged) member with no deps and no rudzio usage.
fn plain(name: &str) -> Member {
    Member {
        name: name.to_owned(),
        deps: Vec::new(),
        bridged: false,
    }
}

/// A direct path dependency `pkg = { path = "../pkg" }`.
fn direct(pkg: &str) -> Dep {
    Dep {
        extern_name: pkg.to_owned(),
        package: pkg.to_owned(),
        kind: DepKind::Direct,
    }
}

/// A renamed path dependency `ext = { package = "pkg", path = "../pkg" }`.
fn renamed(ext: &str, pkg: &str) -> Dep {
    Dep {
        extern_name: ext.to_owned(),
        package: pkg.to_owned(),
        kind: DepKind::Renamed,
    }
}

/// A workspace-inherited dependency `ext = { workspace = true }`.
fn ws(ext: &str, pkg: &str) -> Dep {
    Dep {
        extern_name: ext.to_owned(),
        package: pkg.to_owned(),
        kind: DepKind::Workspace,
    }
}

/// An extra `[workspace.dependencies]` entry keyed `key` naming package `pkg`.
fn ws_root(key: &str, pkg: &str) -> WorkspaceDep {
    WorkspaceDep {
        key: key.to_owned(),
        package: pkg.to_owned(),
    }
}

/// Render one dependency line for a member manifest.
fn dep_line(dep: &Dep) -> String {
    let extern_name = &dep.extern_name;
    let package = &dep.package;
    match dep.kind {
        DepKind::Direct => format!("{extern_name} = {{ path = \"../{package}\" }}\n"),
        DepKind::Renamed => {
            format!("{extern_name} = {{ package = \"{package}\", path = \"../{package}\" }}\n")
        }
        DepKind::Workspace => format!("{extern_name} = {{ workspace = true }}\n"),
    }
}

/// Render an extra `[workspace.dependencies]` entry for the root manifest.
fn workspace_dep_line(ws: &WorkspaceDep) -> String {
    if ws.key == ws.package {
        format!("{} = {{ path = \"crates/{}\" }}\n", ws.key, ws.package)
    } else {
        format!(
            "{} = {{ package = \"{}\", path = \"crates/{}\" }}\n",
            ws.key, ws.package, ws.package
        )
    }
}

/// Build the `lib.rs` for a member.
fn member_lib_rs(member: &Member) -> String {
    let mut src = String::from(
        "pub struct Widget {
    pub value: u32,
}

pub fn make_widget(value: u32) -> Widget {
    Widget { value }
}
",
    );
    if !member.bridged {
        return src;
    }
    src.push('\n');
    src.push_str(&consume_fns(member));
    src.push_str(
        "#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
",
    );
    src.push_str(&member_tests(member));
    src.push_str("    }\n}\n");
    src
}

/// Render the `consume_<dep>` functions a bridged member emits for its deps.
fn consume_fns(member: &Member) -> String {
    let mut out = String::new();
    for dep in &member.deps {
        let extern_name = &dep.extern_name;
        let block = format!(
            "pub fn consume_{extern_name}(widget: &{extern_name}::Widget) -> u32 {{
    widget.value * 2
}}

"
        );
        out.push_str(&block);
    }
    out
}

/// Render the test bodies a bridged member emits.
fn member_tests(member: &Member) -> String {
    if member.deps.is_empty() {
        return format!(
            "        #[rudzio::test]
        fn {name}_self(_t: &super::Test) -> anyhow::Result<()> {{
            anyhow::ensure!(crate::make_widget(1).value == 1);
            Ok(())
        }}
",
            name = member.name,
        );
    }
    let mut out = String::new();
    for dep in &member.deps {
        let extern_name = &dep.extern_name;
        let block = format!(
            "        #[rudzio::test]
        fn {name}_uses_{extern_name}(_t: &super::Test) -> anyhow::Result<()> {{
            let widget = {extern_name}::make_widget(21);
            anyhow::ensure!(crate::consume_{extern_name}(&widget) == 42);
            Ok(())
        }}
",
            name = member.name,
        );
        out.push_str(&block);
    }
    out
}

/// Build the `Cargo.toml` for a member.
fn member_manifest(member: &Member) -> String {
    let mut manifest = format!(
        "[package]
name = \"{name}\"
version = \"0.1.0\"
edition = \"2024\"
publish = false

[lib]
name = \"{name}\"
path = \"src/lib.rs\"
",
        name = member.name,
    );
    if !member.deps.is_empty() {
        manifest.push_str("\n[dependencies]\n");
        for dep in &member.deps {
            manifest.push_str(&dep_line(dep));
        }
    }
    if member.bridged {
        manifest.push_str(
            "\n[dev-dependencies]\nrudzio = { workspace = true }\nanyhow = { workspace = true }\n",
        );
    }
    manifest
}

/// Build the workspace-root `Cargo.toml`.
fn root_manifest(rudzio_root: &Path, workspace_deps: &[WorkspaceDep]) -> String {
    let mut manifest = format!(
        "[workspace]
members = [\"crates/*\"]
resolver = \"3\"

[workspace.dependencies]
rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}
anyhow = \"=1.0.104\"
",
        rudzio = rudzio_root.display(),
    );
    for ws in workspace_deps {
        manifest.push_str(&workspace_dep_line(ws));
    }
    manifest
}

/// Repo root = parent of `cargo-rudzio/`. The fixture's `rudzio`
/// workspace dependency points here so it builds against the code
/// under test.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Materialise the fixture workspace under `root`.
fn write_fixture(
    root: &Path,
    members: &[Member],
    workspace_deps: &[WorkspaceDep],
) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    fs::write(
        root.join("Cargo.toml"),
        root_manifest(rudzio_root, workspace_deps),
    )?;
    for member in members {
        let dir = root.join("crates").join(&member.name);
        fs::create_dir_all(dir.join("src"))?;
        fs::write(dir.join("Cargo.toml"), member_manifest(member))?;
        fs::write(dir.join("src").join("lib.rs"), member_lib_rs(member))?;
    }
    Ok(())
}

/// Build the fixture and run workspace-wide `cargo-rudzio test` (no `-p`).
fn run_workspace_wide(
    cargo: &CargoEnv<'_>,
    members: &[Member],
    workspace_deps: &[WorkspaceDep],
) -> anyhow::Result<Output> {
    let workspace = TempDir::new_in(cargo.tempdir)?;
    write_fixture(workspace.path(), members, workspace_deps)?;
    Ok(
        Command::new(cargo_rudzio_test_helpers::assert_fresh_binary(env!(
            "CARGO_BIN_EXE_cargo-rudzio"
        ))?)
        .arg("test")
        .current_dir(workspace.path())
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo.home)
        .env("CARGO_TARGET_DIR", cargo.target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .output()?,
    )
}

/// Parse the passed leaf names and the `N passed` count from plain-mode
/// stdout.
fn parse_passed(stdout: &str) -> (BTreeSet<String>, Option<usize>) {
    let mut leaves = BTreeSet::new();
    for line in stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix("[OK]") else {
            continue;
        };
        let Some(first) = rest.split_whitespace().next() else {
            continue;
        };
        if first == "setup" || first == "teardown" {
            continue;
        }
        let leaf = first.rsplit("::").next().unwrap_or(first);
        let _newly_inserted: bool = leaves.insert(leaf.to_owned());
    }
    let count = stdout.lines().find_map(parse_passed_count);
    (leaves, count)
}

/// Extract the `N` from a `test result: ok. N passed; …` summary line.
fn parse_passed_count(line: &str) -> Option<usize> {
    let head = line.split(" passed;").next()?;
    head.rsplit(' ').next()?.parse::<usize>().ok()
}

/// Compute the expected passed leaf-name set for `members`.
fn expected_leaves(members: &[Member]) -> BTreeSet<String> {
    let mut leaves = BTreeSet::new();
    for member in members {
        if !member.bridged {
            continue;
        }
        if member.deps.is_empty() {
            let _newly_inserted: bool = leaves.insert(format!("{}_self", member.name));
        } else {
            for dep in &member.deps {
                let _newly_inserted: bool =
                    leaves.insert(format!("{}_uses_{}", member.name, dep.extern_name));
            }
        }
    }
    leaves
}

/// Require exit 0 and the exact expected passed leaf set.
fn assert_passed(output: &Output, members: &[Member]) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        output.status.success(),
        "workspace-wide `cargo rudzio test` must exit 0; exit {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status,
    );
    let (leaves, passed_count) = parse_passed(&stdout);
    let expected = expected_leaves(members);
    anyhow::ensure!(
        leaves == expected && passed_count == Some(expected.len()),
        "expected passed leaf set {expected:?} (count {}), got {leaves:?} (count {passed_count:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        expected.len(),
    );
    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        CargoEnv, IsolatedCargoTest, assert_passed, bridged, direct, plain, renamed,
        run_workspace_wide, ws, ws_root,
    };

    /// Run the `members` fixture against `ctx`'s isolated cargo env and
    /// require exit 0 plus the exact expected passed leaf set.
    macro_rules! check {
        ($ctx:expr, $members:expr, $workspace_deps:expr) => {{
            let output = run_workspace_wide(
                &CargoEnv {
                    tempdir: $ctx.suite_tempdir(),
                    home: $ctx.cargo_home(),
                    target_dir: $ctx.cargo_target_dir(),
                },
                $members,
                $workspace_deps,
            )?;
            assert_passed(&output, $members)
        }};
    }

    #[rudzio::test]
    fn diamond_members_compile_each_dependency_once(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let members = vec![
            bridged("d", vec![]),
            bridged("b", vec![direct("d")]),
            bridged("c", vec![direct("d")]),
            bridged("a", vec![direct("b"), direct("c")]),
        ];
        check!(ctx, &members, &[])
    }

    #[rudzio::test]
    fn chain_members_compile_each_dependency_once(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let members = vec![
            bridged("c", vec![]),
            bridged("b", vec![direct("c")]),
            bridged("a", vec![direct("b")]),
        ];
        check!(ctx, &members, &[])
    }

    #[rudzio::test]
    fn plain_lib_without_rudzio_is_not_bridged(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let members = vec![
            plain("b"),
            bridged("c", vec![direct("b")]),
            bridged("a", vec![direct("b")]),
        ];
        check!(ctx, &members, &[])
    }

    #[rudzio::test]
    fn workspace_dependency_path_dep_is_inherited(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let members = vec![bridged("b", vec![]), bridged("a", vec![ws("b", "b")])];
        let workspace_deps = vec![ws_root("b", "b")];
        check!(ctx, &members, &workspace_deps)
    }

    /// A renamed sibling dep (`b1 = { package = "b", path = "../b" }`) and
    /// a plain sibling dep (`b = { path = "../b" }`) on the same package
    /// coexist across members. Cargo forbids two renames of one package in
    /// a single manifest ("depends on crate `b` multiple times with
    /// different names"), so the second rename is dropped; the remaining
    /// rename + plain pair still exercises the `dep.name` (vs entry-name)
    /// dedup boundary that Bug #6 fixed.
    #[rudzio::test]
    fn renamed_sibling_deps_do_not_duplicate(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let members = vec![
            bridged("b", vec![]),
            bridged("c", vec![direct("b")]),
            bridged("a", vec![renamed("b1", "b")]),
        ];
        check!(ctx, &members, &[])
    }

    #[rudzio::test]
    fn workspace_dependency_rename_is_inherited(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let members = vec![bridged("b", vec![]), bridged("a", vec![ws("bee", "b")])];
        let workspace_deps = vec![ws_root("bee", "b")];
        check!(ctx, &members, &workspace_deps)
    }
}
