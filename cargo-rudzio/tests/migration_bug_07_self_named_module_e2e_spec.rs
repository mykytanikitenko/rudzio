//! End-to-end reproducer for Bug #7: `cargo rudzio test -p project`
//! succeeds for a crate named `project` whose own `src/lib.rs` declares
//! a self-named module `mod project;`.
//!
//! Pre-fix the generated bridge appended
//! `#[cfg(rudzio_test)] extern crate self as project;`, which collides
//! with the member's own `mod project;` (E0260: `project` defined
//! twice). Post-fix the self-alias is skipped and the proxied test
//! source's `use project::project::answer;` is rewritten to
//! `crate::project::answer;`, so the integration test compiles and the
//! runner reports it passed.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/` (where this spec lives). Used
/// as the `path` for the fixture's `rudzio` workspace dependency so the
/// synthetic workspace builds against the rudzio code under test rather
/// than a published version.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Materialise the Bug #7 fixture under `root`: a workspace whose single
/// `project` member declares `pub mod project;` in its lib and exposes
/// `project::answer() -> u32` (returning 42), plus an integration test
/// that references the member crate by name via
/// `use project::project::answer;`.
fn write_fixture(root: &Path) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;

    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"project\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )?;

    let project = root.join("project");
    fs::create_dir_all(project.join("src"))?;
    fs::create_dir_all(project.join("tests").join("integration"))?;
    fs::write(
        project.join("Cargo.toml"),
        r#"[package]
name = "project"
version = "0.1.0"
edition = "2024"
publish = false
autotests = false

[lib]
name = "project"
path = "src/lib.rs"

[[test]]
name = "integration"
path = "tests/integration/mod.rs"
harness = false

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
"#,
    )?;

    // The self-named module: `mod project;` is exactly the name the
    // bridge would otherwise try to `extern crate self as`.
    fs::write(project.join("src").join("lib.rs"), "pub mod project;\n")?;
    fs::write(
        project.join("src").join("project.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n",
    )?;

    // Integration test whose `use project::project::answer;` references
    // the member crate by name. Post-fix the proxy rewrites it to
    // `crate::project::answer;`, which resolves against the bridge's
    // crate root (the member's lib root) without any self-alias.
    fs::write(
        project.join("tests").join("integration").join("mod.rs"),
        r#"// rudzio_test marker
#![allow(clippy::tests_outside_test_module)]

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
mod tests {
    use project::project::answer;

    #[rudzio::test]
    fn self_named_module_answer_resolves() -> anyhow::Result<()> {
        anyhow::ensure!(answer() == 42, "expected answer() == 42");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
"#,
    )?;

    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{Command, IsolatedCargoTest, Path, TempDir, env, write_fixture};

    #[rudzio::test]
    fn self_named_module_member_aggregates_and_passes(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_fixture(root)?;

        let output = Command::new(cargo_rudzio_test_helpers::assert_fresh_binary(env!(
            "CARGO_BIN_EXE_cargo-rudzio"
        ))?)
        .arg("test")
        .arg("-p")
        .arg("project")
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", ctx.cargo_home())
        .env("CARGO_TARGET_DIR", ctx.cargo_target_dir())
        .env("CARGO_NET_OFFLINE", "true")
        .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test -p project must exit 0 for a self-named-module crate; \
             exit = {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code()
        );
        anyhow::ensure!(
            stdout.contains("self_named_module_answer_resolves"),
            "runner output must report the self-named-module test as passed; \
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );

        Ok(())
    }
}
