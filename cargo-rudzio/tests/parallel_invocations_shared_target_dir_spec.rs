//! Reproducer for cargo-rudzio's parallel-invocation contention on a
//! shared `CARGO_TARGET_DIR`.
//!
//! # Symptom
//!
//! When N parallel `cargo rudzio test` invocations share one
//! `CARGO_TARGET_DIR`, each writes to the same
//! `<target>/rudzio-auto-runner/` directory and they race on file
//! removal / creation in `generate::write_runner`. One invocation
//! fails with:
//!
//! ```text
//! cargo-rudzio: removing .../rudzio-auto-runner/Cargo.toml:
//!   No such file or directory (os error 2)
//! ```
//!
//! because another invocation already removed the file in its own
//! `fs::remove_file` call before this one reached the same line.
//!
//! # Real-world trigger
//!
//!  * Developer has `CARGO_TARGET_DIR` set in their shell (e.g.
//!    pointing to an external mount so build artefacts don't fill
//!    `/home`).
//!  * CI matrix runs `cargo rudzio test` against multiple workspaces
//!    concurrently with one shared cache dir.
//!  * Monorepo `cargo rudzio test -p <every-crate>` if rudzio ever
//!    parallelises per-crate invocations.
//!
//! # Contract
//!
//! Two invariants rudzio must hold:
//!
//!  1. Rudzio MUST respect the user's `CARGO_TARGET_DIR` (use it as
//!     the root for build artefacts, not silently divert).
//!  2. Rudzio MUST tolerate any number of concurrent invocations
//!     against the same `CARGO_TARGET_DIR` without clobbering each
//!     other's aggregator state.
//!
//! These are not in conflict: rudzio can place its aggregator in a
//! per-consumer-workspace subdir under the user's
//! `CARGO_TARGET_DIR`, so the user's target dir is respected AND
//! parallel invocations against distinct consumer workspaces never
//! contend.
//!
//! # Reproducer
//!
//! The 9-runtime matrix below dispatches the same test under 9
//! distinct runtimes; rudzio's runner schedules those 9 runtime
//! groups concurrently. Each invocation:
//!
//!  1. Takes the per-test consumer workspace minted by
//!     `IsolatedCargoSuite::context` (a full-recursive copy of the
//!     suite's template — distinct path per runtime instance, so
//!     each subprocess writes to a distinct
//!     `rudzio-auto-runner-<wshash>-...` subdir under the shared
//!     `CARGO_TARGET_DIR`).
//!  2. Computes a process-wide-shared `CARGO_TARGET_DIR` path under
//!     `TMPDIR` (suffixed with this test process's pid to avoid
//!     cross-test-run collisions).
//!  3. Spawns `cargo rudzio test` against its workspace with
//!     `CARGO_TARGET_DIR` set to that shared path.
//!  4. Asserts the spawned process exited 0 AND stderr does not
//!     contain the aggregator-removal race signature.
//!
//! Pre-fix: at least one of the 9 invocations fires the race and the
//! assertion bails with the captured stderr containing the
//! `removing .../rudzio-auto-runner/Cargo.toml: No such file or
//! directory` line.
//!
//! Post-fix: all 9 invocations write to distinct aggregator subdirs
//! under the shared `CARGO_TARGET_DIR` and complete cleanly.
//!
//! # Phase B migration
//!
//! Pre-migration this spec hand-rolled a `write_minimal_consumer`
//! helper that materialised the same minimal workspace shape the
//! Phase B template now provides. The helper is gone; the test
//! reads `ctx.consumer_workspace()` directly. The shared
//! `CARGO_TARGET_DIR` is orthogonal to the per-test consumer copy
//! — exactly the property the spec is supposed to assert.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};

/// One shared `CARGO_TARGET_DIR` path that every runtime instance of
/// this test in the current process sees. Forces the contention case:
/// 9 parallel `cargo rudzio test` subprocesses all writing under the
/// same `<dir>/rudzio-auto-runner/` tree.
///
/// Per-process-pid suffix keeps repeated test runs (and tests run in
/// parallel under `cargo test`) from sharing state across processes.
fn shared_target_dir() -> PathBuf {
    env::temp_dir().join(format!("rudzio-parallel-contention-spec-{}", process::id()))
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = CurrentThread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = Local::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = compio::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = embassy::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = ThreadPool::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = async_std::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = smol::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use std::process::Output;

    use super::{Command, IsolatedCargoTest, Path, PathBuf, Stdio, fs, io, shared_target_dir};

    /// Spawn `cargo rudzio test` in `root` with `CARGO_TARGET_DIR=
    /// shared` set explicitly. Returns the spawned process's
    /// captured `Output`. Pipes both streams so the assertion site
    /// can grep stderr for the race signature.
    ///
    /// `cargo_home` is the per-test private cargo home minted by
    /// [`IsolatedCargoSuite::context`] — concurrent invocations under
    /// the runtime matrix do not contend on the parent test session's
    /// `.package-cache` flock. The `CARGO_TARGET_DIR` this test
    /// deliberately shares across all 9 parallel invocations is
    /// orthogonal: it's the regression class the test guards
    /// against. With `CARGO_NET_OFFLINE=true`, the spawned cargo
    /// resolves entirely from the per-test hardlinked registry.
    fn run_cargo_rudzio_with_shared_target(
        root: &Path,
        shared: &Path,
        cargo_home: &Path,
    ) -> io::Result<Output> {
        let cargo_rudzio_bin =
            cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
                .map_err(io::Error::other)?;
        Command::new(cargo_rudzio_bin)
            .arg("test")
            .current_dir(root)
            .env("CARGO_TARGET_DIR", shared)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_NET_OFFLINE", "true")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }

    /// Parallel-invocation contention reproducer. The 9-runtime
    /// matrix dispatches this test under 9 runtimes concurrently;
    /// each invocation spawns its own `cargo rudzio test` subprocess
    /// against its own per-test consumer workspace (full-recursive
    /// copy of the suite's template — distinct path per runtime
    /// instance), all sharing one `CARGO_TARGET_DIR`. Pre-fix at
    /// least one subprocess hits the
    /// `removing .../Cargo.toml: No such file or directory` race
    /// inside `generate::write_runner`. Post-fix each invocation
    /// writes to a distinct per-consumer aggregator subdir, no race.
    #[rudzio::test]
    async fn parallel_invocations_against_distinct_workspaces_sharing_target_dir_all_succeed(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        let shared: PathBuf = shared_target_dir();
        fs::create_dir_all(&shared)?;

        let output = run_cargo_rudzio_with_shared_target(workspace, &shared, ctx.cargo_home())?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

        let race_signature_present =
            stderr.contains("No such file or directory") && stderr.contains("rudzio-auto-runner");
        anyhow::ensure!(
            !race_signature_present,
            "parallel-invocation contention fired: stderr contains the \
             aggregator-removal race signature (\"No such file or \
             directory\" + \"rudzio-auto-runner\"). Two or more \
             concurrent `cargo rudzio test` invocations clobbered each \
             other's `<target>/rudzio-auto-runner/` state.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
        );
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test exited {:?} (non-zero). Not the canonical \
             race signature, but still a failure — investigate the \
             captured output. May be a follow-on bug surfaced once the \
             race no longer fires.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
            output.status.code(),
        );
        Ok(())
    }
}
