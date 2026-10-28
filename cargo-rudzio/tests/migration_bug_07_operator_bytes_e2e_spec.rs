//! Byte-faithful end-to-end regression for Bug #7 (E0260:
//! `extern crate self as selfmod;` collides with `mod selfmod;`).
//!
//! The operator ran the exact bytes below against the current binary;
//! the original E0260 failure was almost certainly a stale crates.io
//! 0.1.2 binary shadowing the nix-built 0.2.0. This spec pins the
//! operator's exact fixture so the collision cannot regress: a crate
//! named `selfmod` whose own `src/lib.rs` is byte-exact `mod selfmod;\n`
//! (13 B) and whose test resolves the member through the real
//! `crate::selfmod` path.
//!
//! It also closes the follow-up finding: the generated
//! `mod __rudzio_self_alias { pub(crate) use crate as selfmod; }`
//! emits `warning: unused import: crate as selfmod` when no proxied
//! test actually references the member by its crate name. The run
//! output must carry no `unused import` diagnostic.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;
use tempfile::TempDir;

/// The operator's exact `selfmod/src/lib.rs` bytes: 13 B, `mod selfmod;\n`.
const SELF_MOD_LIB_RS: &str = "mod selfmod;\n";

/// The operator's exact `selfmod/src/selfmod.rs` bytes.
const SELF_MOD_RS: &str = "pub fn inner() -> u32 { 42 }\n";

/// The operator's exact `selfmod/tests/smoke.rs` bytes.
const SELF_MOD_SMOKE_RS: &str = "#[rudzio::suite([\n\
    (runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test),\n\
])]\n\
mod tests {\n\
    #[rudzio::test]\n\
    async fn inner_returns_42(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
        anyhow::ensure!(crate::selfmod::inner() == 42);\n\
        Ok(())\n\
    }\n\
}\n";

/// `macrogen/src/lib.rs`: no self-named module, so the bridge must keep
/// the plain `extern crate self as macrogen;` prelude entry.
const MACROGEN_LIB_RS: &str = "pub fn answer() -> u32 { 42 }\n";

/// `macrogen/tests/smoke.rs`: resolves the member by its crate name, so
/// the `extern crate self as macrogen;` alias is both emitted AND used.
const MACROGEN_SMOKE_RS: &str = "#[rudzio::suite([\n\
    (runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test),\n\
])]\n\
mod tests {\n\
    #[rudzio::test]\n\
    async fn answer_is_42(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
        anyhow::ensure!(macrogen::answer() == 42);\n\
        Ok(())\n\
    }\n\
}\n";

/// Repo root = parent of `cargo-rudzio/`; the fixture's `rudzio` path
/// dependency so the synthetic workspace builds against the rudzio code
/// under test rather than a published version.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Lay the operator's two-member workspace down under `root`.
fn write_fixture(root: &Path) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;

    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"selfmod\", \"macrogen\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-futures\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )?;

    write_lib_member(
        root,
        "selfmod",
        SELF_MOD_LIB_RS,
        SELF_MOD_RS,
        SELF_MOD_SMOKE_RS,
    )?;
    write_lib_member(root, "macrogen", MACROGEN_LIB_RS, "", MACROGEN_SMOKE_RS)?;
    Ok(())
}

/// Write a lib-bearing member named `name` with `[dependencies]` on the
/// workspace `rudzio` + `anyhow`. `module_rs` is the sibling
/// `src/<name>.rs` (empty for `macrogen`, which has no self-named
/// module); `lib_rs` is the exact `src/lib.rs`; `smoke_rs` is the exact
/// `tests/smoke.rs`.
fn write_lib_member(
    root: &Path,
    name: &str,
    lib_rs: &str,
    module_rs: &str,
    smoke_rs: &str,
) -> io::Result<()> {
    let member = root.join(name);
    fs::create_dir_all(member.join("src"))?;
    fs::create_dir_all(member.join("tests"))?;
    fs::write(
        member.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"{name}\"\n\
             version = \"0.1.0\"\n\
             edition = \"2024\"\n\
             \n\
             [dependencies]\n\
             rudzio = {{ workspace = true }}\n\
             anyhow = {{ workspace = true }}\n"
        ),
    )?;
    fs::write(member.join("src").join("lib.rs"), lib_rs)?;
    if !module_rs.is_empty() {
        fs::write(member.join("src").join(format!("{name}.rs")), module_rs)?;
    }
    fs::write(member.join("tests").join("smoke.rs"), smoke_rs)?;
    Ok(())
}

/// Spawn `cargo rudzio test` against `root` with the isolated cargo env.
fn run_cargo_rudzio(
    root: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
    args: &[&str],
) -> io::Result<Output> {
    Command::new(
        cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .map_err(io::Error::other)?,
    )
    .arg("test")
    .args(args)
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

/// Locate the generated aggregator directory under `target_dir`: the
/// single `rudzio-auto-runner-<workspace-hash>-<stamp-hash>` subdir
/// `cargo rudzio test` materialises via `Plan::default_output_dir`.
fn find_aggregator_dir(target_dir: &Path) -> anyhow::Result<PathBuf> {
    let mut matches: Vec<PathBuf> = Vec::new();
    for raw_entry in fs::read_dir(target_dir)
        .map_err(|err| anyhow::anyhow!("reading target dir {}: {err}", target_dir.display()))?
    {
        let entry = raw_entry
            .map_err(|err| anyhow::anyhow!("reading target dir {}: {err}", target_dir.display()))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("rudzio-auto-runner-"))
        {
            matches.push(entry.path());
        }
    }
    match matches.as_slice() {
        [single] => Ok(single.clone()),
        [] => anyhow::bail!(
            "no `rudzio-auto-runner-*` aggregator dir under {}",
            target_dir.display()
        ),
        many => anyhow::bail!(
            "expected one aggregator dir under {}, found {}: {many:?}",
            target_dir.display(),
            many.len(),
        ),
    }
}

/// Assert exit 0 and that `expected` appears among the passed test names.
fn assert_exit_0_and_passed(output: &Output, expected: &str) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        output.status.success(),
        "cargo rudzio test must exit 0; exit = {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code(),
    );
    anyhow::ensure!(
        stdout.contains(expected),
        "runner output must report `{expected}` as passed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
    Ok(())
}

/// Assert the run emitted no `unused import` diagnostic anywhere in
/// stdout or stderr (Bug #7 follow-up: the `__rudzio_self_alias` module).
fn assert_no_unused_import(output: &Output) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        !stdout.contains("unused import") && !stderr.contains("unused import"),
        "run output must not contain an `unused import` warning\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
    Ok(())
}

#[rudzio::suite([(runtime = FuturesThreadPool::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, TempDir, assert_exit_0_and_passed, assert_no_unused_import,
        find_aggregator_dir, fs, run_cargo_rudzio, write_fixture,
    };

    #[rudzio::test]
    fn operator_bytes_p_selfmod_passes_without_unused_import(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root)?;

        let output = run_cargo_rudzio(
            root,
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &["-p", "selfmod"],
        )?;
        assert_exit_0_and_passed(&output, "selfmod::smoke::inner_returns_42")?;
        assert_no_unused_import(&output)?;

        let aggregator = find_aggregator_dir(ctx.cargo_target_dir())?;
        let selfmod_bridge = aggregator
            .join("members")
            .join("selfmod")
            .join("src")
            .join("lib.rs");
        let content = fs::read_to_string(&selfmod_bridge)?;
        anyhow::ensure!(
            !content.contains("extern crate self as selfmod;"),
            "bridge lib.rs must NOT emit `extern crate self as selfmod;` for a crate named \
             `selfmod` whose own lib.rs declares `mod selfmod;` (E0260). Bug #7.\n\
             --- {} ---\n{content}",
            selfmod_bridge.display(),
        );
        Ok(())
    }

    #[rudzio::test]
    fn workspace_wide_macrogen_bridge_keeps_self_alias(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root)?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir(), &[])?;
        assert_exit_0_and_passed(&output, "selfmod::smoke::inner_returns_42")?;
        assert_exit_0_and_passed(&output, "macrogen::smoke::answer_is_42")?;
        assert_no_unused_import(&output)?;

        let aggregator = find_aggregator_dir(ctx.cargo_target_dir())?;
        let selfmod_bridge = aggregator
            .join("members")
            .join("selfmod")
            .join("src")
            .join("lib.rs");
        let macrogen_bridge = aggregator
            .join("members")
            .join("macrogen")
            .join("src")
            .join("lib.rs");

        let selfmod_content = fs::read_to_string(&selfmod_bridge)?;
        anyhow::ensure!(
            !selfmod_content.contains("extern crate self as selfmod;"),
            "bridge lib.rs must NOT emit `extern crate self as selfmod;` for a self-named \
             `selfmod` crate (E0260). Bug #7.\n--- {} ---\n{selfmod_content}",
            selfmod_bridge.display(),
        );

        let macrogen_content = fs::read_to_string(&macrogen_bridge)?;
        anyhow::ensure!(
            macrogen_content.contains("extern crate self as macrogen;"),
            "bridge lib.rs must emit `extern crate self as macrogen;` for a non-self-named \
             crate whose test references it by crate name\n--- {} ---\n{macrogen_content}",
            macrogen_bridge.display(),
        );
        Ok(())
    }
}
