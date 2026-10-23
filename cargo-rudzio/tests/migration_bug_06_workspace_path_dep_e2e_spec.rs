//! End-to-end reproducer for Bug #6: a workspace-wide `cargo rudzio
//! test` (no `-p`) compiles a path-dependency workspace member twice.
//!
//! # Symptom (pre-fix, cargo-rudzio 0.1.2)
//!
//! Workspace `members = ["crates/*"]`, `crates/a` path-depends on
//! `crates/b`, both carry rudzio tests. The aggregator builds `b` once
//! through its bridge (`members/b`) and once more through `crates/b`,
//! so rustc reports "multiple different versions of crate `b`" and
//! every place where a `b` type crosses into `a` fails to type-check.
//!
//! # Reproduction
//!
//! Each fixture variant wires `a -> b` differently (plain normal dep,
//! renamed normal dep, dev-dependency). `a`'s test calls a `b` function
//! and hands a `b` type to an `a` function, so a duplicated `b` cannot
//! compile. `cargo-rudzio test` runs from the workspace root with no
//! `-p`; the run must exit 0 and report both members' tests as passed.
#![cfg(unix)]

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// `crates/b/src/lib.rs`: a type and constructor `a` consumes, plus
/// `b`'s own rudzio test.
const B_LIB_RS: &str = "// rudzio_test marker
pub struct Widget {
    pub value: u32,
}

pub fn make_widget(value: u32) -> Widget {
    Widget { value }
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn b_builds_widget(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(crate::make_widget(7).value == 7);
            Ok(())
        }
    }
}
";

/// `crates/b/Cargo.toml`.
const B_MANIFEST: &str = r#"[package]
name = "b"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
name = "b"
path = "src/lib.rs"

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
"#;

/// How member `a` reaches member `b` in a fixture variant.
#[derive(Clone, Copy)]
enum Wiring {
    /// `[dev-dependencies] b = { path = "../b" }`.
    Dev,
    /// `[dependencies] b = { path = "../b" }`.
    PlainNormal,
    /// `[dependencies] bee = { package = "b", path = "../b" }`.
    RenamedNormal,
}

impl Wiring {
    /// Name `a`'s source uses to refer to crate `b`.
    const fn extern_name(self) -> &'static str {
        match self {
            Self::Dev | Self::PlainNormal => "b",
            Self::RenamedNormal => "bee",
        }
    }
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

/// `crates/a/src/lib.rs` for `wiring`. With a dev-dependency `b` is
/// only visible under test cfgs, so the `b`-typed function is gated the
/// same way.
fn a_lib_rs(wiring: Wiring) -> String {
    let crate_name = wiring.extern_name();
    let gate = match wiring {
        Wiring::PlainNormal | Wiring::RenamedNormal => "",
        Wiring::Dev => "#[cfg(any(test, rudzio_test))]\n",
    };
    format!(
        "// rudzio_test marker
{gate}pub fn doubled(widget: &{crate_name}::Widget) -> u32 {{
    widget.value * 2
}}

#[cfg(any(test, rudzio_test))]
mod tests {{
    use rudzio::common::context::{{Suite, Test}};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {{
        #[rudzio::test]
        fn a_consumes_b_type(_t: &super::Test) -> anyhow::Result<()> {{
            let widget = {crate_name}::make_widget(21);
            anyhow::ensure!(crate::doubled(&widget) == 42);
            Ok(())
        }}
    }}
}}
"
    )
}

/// `crates/a/Cargo.toml` for `wiring`.
fn a_manifest(wiring: Wiring) -> String {
    let (deps, dev_extra) = match wiring {
        Wiring::PlainNormal => ("b = { path = \"../b\" }\n", ""),
        Wiring::RenamedNormal => ("bee = { package = \"b\", path = \"../b\" }\n", ""),
        Wiring::Dev => ("", "b = { path = \"../b\" }\n"),
    };
    format!(
        r#"[package]
name = "a"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
name = "a"
path = "src/lib.rs"

[dependencies]
{deps}
[dev-dependencies]
{dev_extra}rudzio = {{ workspace = true }}
anyhow = {{ workspace = true }}
"#
    )
}

/// The run exited 0 and reported both members' tests as passed.
fn ensure_both_members_passed(output: &Output) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = |name: &str| {
        stdout
            .lines()
            .any(|line| line.contains(name) && line.contains("ok"))
    };
    anyhow::ensure!(
        output.status.success() && passed("a_consumes_b_type") && passed("b_builds_widget"),
        "workspace-wide `cargo rudzio test` must compile `b` once and pass both members' \
         tests; exit {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status,
    );
    Ok(())
}

/// Repo root = parent of `cargo-rudzio/`. The fixture's `rudzio`
/// workspace dependency points here so it builds against the code
/// under test.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Build the `wiring` fixture and run workspace-wide `cargo-rudzio test`.
fn run_workspace_wide(cargo: &CargoEnv<'_>, wiring: Wiring) -> anyhow::Result<Output> {
    let workspace = TempDir::new_in(cargo.tempdir)?;
    let root = workspace.path();
    write_fixture(root, wiring)?;
    Ok(Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
        .arg("test")
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

/// Materialise the Bug #6 fixture for `wiring` under `root`.
fn write_fixture(root: &Path, wiring: Wiring) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["crates/*"]
resolver = "3"

[workspace.dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;
    let a_dir = root.join("crates").join("a");
    let b_dir = root.join("crates").join("b");
    fs::create_dir_all(a_dir.join("src"))?;
    fs::create_dir_all(b_dir.join("src"))?;
    fs::write(a_dir.join("Cargo.toml"), a_manifest(wiring))?;
    fs::write(a_dir.join("src").join("lib.rs"), a_lib_rs(wiring))?;
    fs::write(b_dir.join("Cargo.toml"), B_MANIFEST)?;
    fs::write(b_dir.join("src").join("lib.rs"), B_LIB_RS)?;
    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        CargoEnv, IsolatedCargoTest, Wiring, ensure_both_members_passed, run_workspace_wide,
    };

    /// Run the `wiring` fixture against `ctx`'s isolated cargo env and
    /// require both members' tests to pass.
    macro_rules! check {
        ($ctx:expr, $wiring:expr) => {
            ensure_both_members_passed(&run_workspace_wide(
                &CargoEnv {
                    tempdir: $ctx.suite_tempdir(),
                    home: $ctx.cargo_home(),
                    target_dir: $ctx.cargo_target_dir(),
                },
                $wiring,
            )?)
        };
    }

    #[rudzio::test]
    fn dev_path_dep_compiles_member_once(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        check!(ctx, Wiring::Dev)
    }

    #[rudzio::test]
    fn plain_normal_path_dep_compiles_member_once(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        check!(ctx, Wiring::PlainNormal)
    }

    #[rudzio::test]
    fn renamed_normal_path_dep_compiles_member_once(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        check!(ctx, Wiring::RenamedNormal)
    }
}
