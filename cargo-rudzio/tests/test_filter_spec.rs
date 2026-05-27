//! Behavioral spec: `cargo rudzio test --test <name>` actually
//! filters which `[[test]]` binaries the aggregator compiles
//! (Bug #3).
//!
//! # Symptom (pre-fix, cargo-rudzio 0.1.1)
//!
//! `parse_target_selection_flags` consumes `--test <name>` but does
//! NOT honor it: every test binary in the workspace lands in the
//! aggregator. A workspace whose `broken` `[[test]]` doesn't compile
//! breaks `cargo rudzio test --test good` even though the user asked
//! for ONLY `good`.
//!
//! # Reproduction
//!
//! Fixture: one consumer member with two `[[test]]` binaries. `good`
//! is well-formed; `broken` references an undefined symbol. Invoke
//! the cargo-rudzio binary as a subprocess with
//! `test --test good --no-run`. Pre-fix: exit code != 0 because
//! cargo fails compiling `broken`. Post-fix: exit code == 0 because
//! `broken` was filtered out at aggregation time.
//!
//! Subprocess approach (not library-level) is mandatory because the
//! filter belongs at the CLI → `Plan` boundary, which a library
//! caller bypasses; only the binary exercises the full flow.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Workspace root of the in-tree rudzio package (one level above
/// `manifest_dir!()` which points at the rudzio crate itself).
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a single-member workspace with a compiling `good`
/// test binary and a deliberately-broken `broken` one.
///
/// The fixture path-pins rudzio to the in-tree source so no registry
/// fetch is required when the subprocess runs cargo against it.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"member\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", default-features = false, features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    let member = root.join("member");
    fs::create_dir_all(member.join("src"))?;
    fs::create_dir_all(member.join("tests").join("good"))?;
    fs::create_dir_all(member.join("tests").join("broken"))?;
    fs::write(
        member.join("Cargo.toml"),
        "[package]\n\
         name = \"member\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"member\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[test]]\n\
         name = \"good\"\n\
         path = \"tests/good/mod.rs\"\n\
         harness = false\n\
         \n\
         [[test]]\n\
         name = \"broken\"\n\
         path = \"tests/broken/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n",
    )?;
    fs::write(member.join("src").join("lib.rs"), "// rudzio_test marker\n")?;
    fs::write(
        member.join("tests").join("good").join("mod.rs"),
        "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
    )?;
    // `undefined_symbol` doesn't resolve. If the filter actually
    // applies at aggregation time, this file is NEVER compiled and
    // the build succeeds. If the filter is silently dropped, cargo
    // tries to compile this file and the build fails — which is
    // exactly the pre-fix Bug #3 symptom.
    fs::write(
        member.join("tests").join("broken").join("mod.rs"),
        "// rudzio_test marker\n#[rudzio::main]\nfn main() { let _: () = undefined_symbol; }\n",
    )?;
    Ok(())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{Command, IsolatedCargoTest, TempDir, env, rudzio_source_root, write_fixture};

    #[rudzio::test]
    fn cargo_rudzio_test_with_test_filter_skips_other_binaries(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let temp = TempDir::new_in(ctx.suite_tempdir())?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let cargo_rudzio = env!("CARGO_BIN_EXE_cargo-rudzio");
        // Per-test cargo state (hardlinked from suite's pre-warmed
        // registry) + offline mode = sibling tests do not contend on
        // the `.package-cache` flock and the aggregator dir lives
        // inside this test's tempdir (auto-cleaned at drop).
        let output = Command::new(cargo_rudzio)
            .arg("test")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--test")
            .arg("good")
            .arg("--no-run")
            .env(
                "CARGO",
                env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
            )
            .env("CARGO_HOME", ctx.cargo_home())
            .env("CARGO_TARGET_DIR", ctx.cargo_target_dir())
            .env("CARGO_NET_OFFLINE", "true")
            .output()?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "`cargo-rudzio test --test good --no-run` exited {status:?}: \
                 the `broken` binary was compiled despite `--test good`. \
                 stdout:\n{stdout}\nstderr:\n{stderr}",
                status = output.status.code(),
            );
        }
        Ok(())
    }
}
