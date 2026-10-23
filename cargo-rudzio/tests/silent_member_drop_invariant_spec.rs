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
//! # Phase B migration
//!
//! Starts from `ctx.consumer_workspace()` (the suite's template) and
//! adds:
//!  1. A `broken-helper/` sibling crate with deliberately malformed
//!     `Cargo.toml` (missing `version` field).
//!  2. A `[dev-dependencies] broken-helper = { path = "../broken-helper" }`
//!     entry appended to the template's `consumer/Cargo.toml`.
//!
//! Both mutations land on the per-test copy; the suite's template
//! is untouched.
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
use std::path::Path;
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// Extend the template-provided consumer workspace at `workspace`
/// with the `broken-helper/` sibling crate the silent-member-drop
/// invariant probes. The template-provided files are left in place
/// (the trivial `#[rudzio::test]` in `consumer/src/lib.rs` is the
/// baseline test rudzio must NOT drop alongside the broken
/// helper); only the additions below land.
///
/// Additions:
///
///  * `broken-helper/Cargo.toml` — deliberately omits the required
///    `version` field so `cargo metadata --manifest-path
///    <broken-helper>/Cargo.toml` rejects it. The
///    `[package.metadata.rudzio].force_bridge = true` entry forces
///    `extend_with_transitive_path_dep_members` to descend into
///    this package even though no rudzio surface in
///    `consumer/src/lib.rs` reaches into it.
///  * `broken-helper/src/lib.rs` — carries the
///    `broken_helper_marker_test` whose presence in the captured
///    output is one of the two accept signals.
///  * `consumer/Cargo.toml` — append a `[dev-dependencies]
///    broken-helper = { path = "../broken-helper" }` line. The
///    template's existing `[dev-dependencies]` rudzio + anyhow
///    stay; append (vs. overwrite) keeps the template content
///    intact.
fn extend_with_broken_helper(workspace: &Path) -> io::Result<()> {
    let broken = workspace.join("broken-helper");
    let broken_src = broken.join("src");
    fs::create_dir_all(&broken_src)?;

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

    // Append the broken-helper dev-dep to the template's existing
    // consumer/Cargo.toml. The append is a single trailing line
    // under the existing `[dev-dependencies]` table — toml-valid
    // because no new section header intervenes.
    let consumer_manifest_path = workspace.join("consumer").join("Cargo.toml");
    let mut consumer_manifest = fs::read_to_string(&consumer_manifest_path)?;
    consumer_manifest.push_str("broken-helper = { path = \"../broken-helper\" }\n");
    fs::write(&consumer_manifest_path, consumer_manifest)?;

    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{Command, IsolatedCargoTest, Output, Path, Stdio, extend_with_broken_helper, io};

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
    ///     contains the template's trivial test but not the
    ///     `broken_helper_marker_test` AND no stderr mention of
    ///     `broken-helper`. This is the silent-member-drop
    ///     symptom — the broken helper vanished from the plan
    ///     without a trace.
    #[rudzio::test]
    async fn broken_transitive_path_dep_metadata_does_not_silently_drop_member(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        extend_with_broken_helper(workspace)?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
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
