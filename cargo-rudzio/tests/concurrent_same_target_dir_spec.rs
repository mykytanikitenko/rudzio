//! EDGE-CASE e2e coverage: concurrent `cargo rudzio test` invocations
//! against the SAME workspace AND the SAME `CARGO_TARGET_DIR`.
//!
//! # Gap this closes
//!
//! `parallel_invocations_shared_target_dir_spec` covers N concurrent
//! invocations against *distinct* consumer workspaces sharing one
//! `CARGO_TARGET_DIR` (each lands in a distinct aggregator subdir), and
//! `edge_build_and_lifecycle_e2e_spec` case B3 covers two concurrent
//! invocations against the *same* workspace with *distinct* task-local
//! `CARGO_TARGET_DIR`s. Neither covers the remaining cell: the same
//! workspace resolving to the *same* `aggregator_subdir_name` (same
//! workspace hash + same build stamp), so every invocation writes its
//! generated aggregator into the SAME `<target>/rudzio-auto-runner-…/`
//! directory.
//!
//! # The race
//!
//! `generate::write_runner` regenerates `Cargo.toml`, `src/main.rs`,
//! `build.rs`, and the `members/` bridge crates on every invocation.
//! Cargo's build lock serialises the *build* phase (all three inner
//! `cargo build` invocations share the per-aggregator target dir), but
//! it does NOT serialise the *generation* phase — `write_runner` runs
//! before the inner cargo build and is unguarded. Two invocations can
//! therefore interleave `remove_dir_all(src)` / `fs::write(Cargo.toml)`
//! while a third's in-flight cargo build is reading those same files,
//! surfacing as a truncated `Cargo.toml` (parse error), a missing
//! `members/…/src/lib.rs` (compile error), or a half-written runner.
//!
//! # What this asserts
//!
//! Three invocations, each filtered to a *different* test, are spawned
//! concurrently against one workspace + one shared `CARGO_TARGET_DIR`.
//! Every invocation must exit 0 AND report exactly its own filtered
//! test — proving no invocation observed a half-written generated file
//! and no invocation's aggregator picked up another invocation's
//! surface. Distinct filters make cross-contamination observable: if
//! the generated aggregators interleave, a run reports the wrong test
//! set rather than its own filter.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// The three test names each concurrent invocation filters to, and the
/// leaf names the runner reports back for them. Distinct and
/// non-overlapping so a substring filter can never match more than one.
const FILTERS: [&str; 3] = ["concurrent_alpha", "concurrent_beta", "concurrent_gamma"];

/// `consumer/src/lib.rs` carrying three `#[rudzio::test]` functions so
/// three concurrent invocations can each filter to a different one.
/// Mirrors the template's imports and suite shape exactly.
const MULTI_TEST_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn concurrent_alpha(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(1 + 1 == 2);
            Ok(())
        }

        #[rudzio::test]
        fn concurrent_beta(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(2 + 2 == 4);
            Ok(())
        }

        #[rudzio::test]
        fn concurrent_gamma(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(3 + 3 == 6);
            Ok(())
        }
    }
}
";

/// Shared `CARGO_TARGET_DIR` for the three concurrent invocations of a
/// single test run. Anchored under the suite tempdir (unique per suite
/// instance) so the three subprocesses contend on the same aggregator
/// dir while sibling suites/tests never collide.
fn shared_target_dir(suite_tempdir: &Path) -> PathBuf {
    suite_tempdir.join("concurrent-same-target-dir")
}

/// Spawn one `cargo rudzio test <filter>` invocation against `workspace`
/// with `CARGO_TARGET_DIR` pinned to `shared` and the per-test private
/// cargo home. Returns the live child so all three overlap.
fn spawn_run(
    bin: &Path,
    workspace: &Path,
    shared: &Path,
    cargo_home: &Path,
    filter: &str,
) -> io::Result<Child> {
    Command::new(bin)
        .arg("test")
        .arg(filter)
        .current_dir(workspace)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", shared)
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
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

/// Assert a single invocation exited 0 and reported exactly its own
/// filtered test — and none of the other two (no cross-contamination
/// from a half-written / interleaved aggregator).
fn assert_run_reports_only(output: &Output, filter: &str) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    anyhow::ensure!(
        output.status.success(),
        "invocation filtered to `{filter}` exited {:?}\n{context}",
        output.status.code(),
    );
    let leaves = passed_leaf_names(&stdout);
    anyhow::ensure!(
        leaves.contains(filter),
        "invocation filtered to `{filter}` did not report it; got {leaves:?}\n{context}",
    );
    for other in FILTERS {
        if other == filter {
            continue;
        }
        anyhow::ensure!(
            !leaves.contains(other),
            "invocation filtered to `{filter}` also reported `{other}` — the generated \
             aggregator was cross-contaminated by a concurrent invocation\n{context}",
        );
    }
    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use std::fs;
    use std::path::Path;

    use anyhow::Context as _;

    use super::{
        FILTERS, IsolatedCargoTest, MULTI_TEST_LIB_RS, assert_run_reports_only, shared_target_dir,
        spawn_run,
    };

    /// Three concurrent `cargo rudzio test` invocations against the same
    /// workspace + same `CARGO_TARGET_DIR`, each filtered to a different
    /// test. All three must exit 0 and report exactly their own test.
    #[rudzio::test]
    fn concurrent_same_workspace_same_target_dir_all_succeed(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        fs::write(
            workspace.join("consumer").join("src").join("lib.rs"),
            MULTI_TEST_LIB_RS,
        )
        .with_context(|| {
            format!(
                "overwriting consumer lib.rs with three-test fixture at {}",
                workspace
                    .join("consumer")
                    .join("src")
                    .join("lib.rs")
                    .display()
            )
        })?;
        let shared = shared_target_dir(ctx.suite_tempdir());
        fs::create_dir_all(&shared)?;
        let bin =
            cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;

        let mut children = Vec::new();
        for filter in FILTERS {
            children.push(
                spawn_run(&bin, workspace, &shared, ctx.cargo_home(), filter)
                    .with_context(|| format!("spawning `cargo rudzio test {filter}`"))?,
            );
        }

        let mut outputs = Vec::new();
        for child in children {
            outputs.push(child.wait_with_output()?);
        }

        for (filter, output) in FILTERS.iter().zip(outputs.iter()) {
            assert_run_reports_only(output, filter)?;
        }
        Ok(())
    }
}
