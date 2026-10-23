//! Bug #5 through the full `cargo rudzio test` pipeline: a workspace member
//! whose `#[rudzio::test]` surface is produced by `macro_rules!` must be
//! aggregated and run end-to-end — the member lib is compiled through the
//! bridge under `--cfg rudzio_test`, the macro-generated test registers via
//! the `#[rudzio::test]` attribute, and the runner reports it.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/`.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// A workspace whose `app` member has two `macro_rules!`-generated tests and
/// one direct fn in the same `#[rudzio::suite]`, all in the member's lib
/// (the bridge compiles lib source, so only lib-compile-unit registrations
/// reach the aggregator).
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["app"]
resolver = "3"

[workspace.dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;

    let app = root.join("app");
    fs::create_dir_all(app.join("src"))?;
    fs::write(
        app.join("Cargo.toml"),
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
name = "app"
path = "src/lib.rs"

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
"#,
    )?;

    fs::write(
        app.join("src").join("lib.rs"),
        "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    macro_rules! make_test {
        ($name:ident) => {
            #[rudzio::test]
            fn $name(_ctx: &Test) -> anyhow::Result<()> {
                Ok(())
            }
        };
    }

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        use super::Test;

        make_test!(macro_generated_a);
        make_test!(macro_generated_b);

        #[rudzio::test]
        fn direct_fn(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    }
}
",
    )?;

    Ok(())
}

#[rudzio::suite([(
    runtime = TokioMultithread::new,
    suite = IsolatedCargoSuite,
    test = IsolatedCargoTest,
)])]
mod tests {
    use super::{
        Command, IsolatedCargoTest, Output, Path, TempDir, env, io, rudzio_source_root,
        write_fixture,
    };

    /// Spawn `cargo rudzio test` against the fixture at `root` with the
    /// isolated cargo env.
    fn run_cargo_rudzio(
        cargo_home: &Path,
        cargo_target_dir: &Path,
        root: &Path,
    ) -> io::Result<Output> {
        Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .arg("test")
            .current_dir(root)
            .env(
                "CARGO",
                env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
            )
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", cargo_target_dir)
            .env("CARGO_NET_OFFLINE", "true")
            .output()
    }

    #[rudzio::test]
    fn macro_generated_tests_run_through_cargo_rudzio(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        let rudzio_root = rudzio_source_root().ok_or_else(|| {
            anyhow::anyhow!("cargo-rudzio/tests must live under the workspace root")
        })?;
        write_fixture(root, rudzio_root)?;

        let output = run_cargo_rudzio(ctx.cargo_home(), ctx.cargo_target_dir(), root)?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::ensure!(
            output.status.success(),
            "expected macro-generated tests to run and pass through cargo rudzio test, but it \
             exited {:?}.\n{combined}",
            output.status.code(),
        );
        anyhow::ensure!(
            combined.contains("macro_generated_a"),
            "output:\n{combined}"
        );
        anyhow::ensure!(
            combined.contains("macro_generated_b"),
            "output:\n{combined}"
        );
        anyhow::ensure!(combined.contains("direct_fn"), "output:\n{combined}");
        anyhow::ensure!(combined.contains("3 passed"), "output:\n{combined}");
        anyhow::ensure!(combined.contains("0 failed"), "output:\n{combined}");
        Ok(())
    }
}
