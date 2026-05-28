//! Behavioural reproducer for the silent-member-drop defect class:
//! a transitive path-dep whose `cargo metadata` call fails silently
//! drops the WHOLE subtree of members from the rudzio plan.
//!
//! # Defect
//!
//! `cargo-rudzio/src/generate.rs:1035` — `extend_with_transitive_path_dep_members`:
//!
//! ```rust,ignore
//! let Ok(sub_metadata) = MetadataCommand::new()
//!     .manifest_path(&manifest)
//!     .no_deps()
//!     .exec()
//! else {
//!     continue;
//! };
//! ```
//!
//! The `let Ok else continue` pattern silently swallows any
//! `MetadataCommand::exec()` failure on a transitive path-dep
//! manifest. The dropped subtree's members never enter the
//! `members` vector, so any `#[rudzio::test]` they contain never
//! reaches the dispatcher.
//!
//! Per the project's zero-tolerance-for-silent-failures rule, this
//! must surface as a hard error citing the offending manifest path
//! — the user needs to know which path-dep rudzio could not read
//! so they can fix it, not get a cryptic cargo error from a
//! downstream compile stage.
//!
//! # Invariant under test
//!
//! When a workspace member declares a path-dependency on a non-
//! workspace package, and that package's `Cargo.toml` is malformed
//! enough that `cargo metadata` cannot read it, `cargo rudzio test`
//! MUST exit non-zero AND emit a stderr message identifying the
//! broken package's manifest path. Silently producing a green run
//! with the broken package's tests absent is FORBIDDEN.
//!
//! # Note on this being a separate spec file
//!
//! The behavioural pattern (build fixture, spawn cargo-rudzio test,
//! classify outcome) is the same as `surface_visibility_invariant_spec`,
//! but the defect CLASS is different — that spec tests "marker
//! in unsupported location", this one tests "marker in supported
//! location that rudzio can't see because it gave up reading
//! metadata". Different root cause, different fix surface, so the
//! tests live apart for clarity.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;
use tempfile::TempDir;

/// Workspace root of the in-tree `rudzio` crate.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a workspace where:
///
///  * `consumer/` is a workspace member with a normal `[lib]` + one
///    `#[rudzio::test]` baseline.
///  * `broken-helper/` is a NON-workspace path-dep declared by
///    `consumer/Cargo.toml [dev-dependencies]`. Its manifest is
///    malformed in a way `cargo metadata` rejects (missing
///    `version` field — required by the manifest format).
///  * `broken-helper/src/lib.rs` carries a rudzio surface marker
///    (`// rudzio_test`) + `[package.metadata.rudzio].force_bridge = true`
///    so `extend_with_transitive_path_dep_members` is the function
///    walking it.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n\
             anyhow = \"=1.0.102\"\n",
            rudzio = rudzio_root.display(),
        ),
    )?;

    // ---- consumer (workspace member) ----
    let consumer = root.join("consumer");
    let consumer_src = consumer.join("src");
    fs::create_dir_all(&consumer_src)?;

    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n\
         broken-helper = { path = \"../broken-helper\" }\n",
    )?;

    fs::write(
        consumer_src.join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn consumer_baseline_test(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    // ---- broken-helper (non-workspace path-dep with broken manifest) ----
    let broken = root.join("broken-helper");
    let broken_src = broken.join("src");
    fs::create_dir_all(&broken_src)?;

    // Manifest deliberately missing `version` — a required field.
    // `cargo metadata --manifest-path <broken>/Cargo.toml` will
    // reject this; the bug is that rudzio's
    // `extend_with_transitive_path_dep_members` swallows the
    // failure with `let Ok else continue` and the marker test
    // below vanishes from the run.
    //
    // `[package.metadata.rudzio].force_bridge = true` forces
    // `extend_with_transitive_path_dep_members` to descend into
    // this package even though it has no rudzio surface that the
    // outer member's bridge would otherwise reach for.
    fs::write(
        broken.join("Cargo.toml"),
        "[package]\n\
         name = \"broken-helper\"\n\
         edition = \"2024\"\n\
         \n\
         [package.metadata.rudzio]\n\
         force_bridge = true\n\
         \n\
         [lib]\n\
         name = \"broken_helper\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dependencies]\n\
         rudzio = { workspace = true }\n",
    )?;

    fs::write(
        broken_src.join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn broken_helper_marker_test(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        Command, IsolatedCargoTest, Output, Path, PathBuf, Stdio, TempDir, io, rudzio_source_root,
        write_fixture,
    };

    /// Spawn `cargo-rudzio test` with per-test isolated cargo state.
    fn run_cargo_rudzio_test(
        root: &Path,
        cargo_home: &Path,
        cargo_target_dir: &Path,
    ) -> io::Result<Output> {
        let cargo_rudzio_bin: &str = env!("CARGO_BIN_EXE_cargo-rudzio");
        Command::new(cargo_rudzio_bin)
            .arg("test")
            .current_dir(root)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", cargo_target_dir)
            .env("CARGO_NET_OFFLINE", "true")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }

    /// **Silent-member-drop core**: a transitive path-dep whose
    /// `cargo metadata` fails must NOT be silently dropped from
    /// the rudzio plan. The fixture's `broken-helper/Cargo.toml`
    /// omits the required `version` field; `cargo metadata` on
    /// it fails; `extend_with_transitive_path_dep_members:1035`
    /// silently `continue`s past the failure.
    ///
    /// Acceptable outcomes for this invariant (any one):
    ///
    ///  1. The toolchain reads `broken-helper` somehow and runs
    ///     its `broken_helper_marker_test`. Unlikely given the
    ///     manifest is genuinely malformed, but pinned for
    ///     completeness.
    ///  2. `cargo rudzio test` exits non-zero AND stderr names
    ///     `broken-helper` (path, package name, or marker name) so
    ///     the user knows which path-dep was the problem.
    ///
    /// Unacceptable (fails the test):
    ///
    ///  3. `cargo rudzio test` exits ZERO with a green run that
    ///     contains `consumer_baseline_test` but not the
    ///     `broken_helper_marker_test` AND no stderr mention of
    ///     `broken-helper`. This is the silent-member-drop
    ///     symptom — the broken helper vanished from the plan
    ///     without a trace.
    #[rudzio::test]
    async fn broken_transitive_path_dep_metadata_does_not_silently_drop_member(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        let rudzio_root: PathBuf = rudzio_source_root();
        write_fixture(root, &rudzio_root)?;

        let output = run_cargo_rudzio_test(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let marker_ran = stdout.contains("broken_helper_marker_test");
        let broken_named_in_stderr = stderr.contains("broken-helper")
            || stderr.contains("broken_helper")
            || stderr.contains("broken_helper_marker_test");

        if output.status.success() && marker_ran {
            return Ok(());
        }
        if !output.status.success() && broken_named_in_stderr {
            return Ok(());
        }

        // Silent-member-drop branch: exit zero AND marker absent
        // AND stderr doesn't mention `broken-helper`. That's the
        // failure mode this test exists to catch.
        if output.status.success() && !marker_ran && !broken_named_in_stderr {
            anyhow::bail!(
                "silent-member-drop invariant violated: `cargo rudzio test` exited \
                 successfully, `broken_helper_marker_test` is absent from the \
                 captured output, and no stderr message mentions `broken-helper`. \
                 The transitive path-dep was silently dropped from the rudzio plan \
                 because its `cargo metadata` call failed and \
                 `extend_with_transitive_path_dep_members:1035` swallowed the \
                 error with `let Ok else continue`. Per the zero-silent-failure \
                 rule this MUST surface as a hard error citing the broken \
                 manifest path so the user can fix it.\n\
                 --- captured stdout ---\n{stdout}\n\
                 --- captured stderr ---\n{stderr}"
            );
        }

        // Catch-all: failure mode that's neither silent-drop nor
        // a clean loud-refusal nor visibility. Surface it for
        // manual inspection so the test stays honest.
        anyhow::bail!(
            "unexpected outcome from `cargo rudzio test`: exit={status:?}, \
             marker_ran={marker_ran}, broken_named_in_stderr={broken_named_in_stderr}. \
             Investigate before treating this test as a pass/fail signal.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
            status = output.status.code(),
        );
    }
}
