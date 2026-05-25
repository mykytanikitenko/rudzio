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
//!  1. Creates a fresh distinct synthetic consumer workspace in its
//!     own tempdir.
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

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Stdio};

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
use tempfile::TempDir;

/// Workspace root of the rudzio repo this test lives in. The
/// synthetic fixture references the in-tree `rudzio` crate by
/// `path = <this>` so it exercises the code under test, not a
/// published version.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// One shared `CARGO_TARGET_DIR` path that every runtime instance of
/// this test in the current process sees. Forces the contention case:
/// 9 parallel `cargo rudzio test` subprocesses all writing under the
/// same `<dir>/rudzio-auto-runner/` tree.
///
/// Per-process-pid suffix keeps repeated test runs (and tests run in
/// parallel under `cargo test`) from sharing state across processes.
fn shared_target_dir() -> PathBuf {
    env::temp_dir().join(format!(
        "rudzio-parallel-contention-spec-{}",
        process::id()
    ))
}

/// Materialise a minimal valid rudzio consumer workspace at `root`.
///
/// One workspace with one `consumer` member, one trivial
/// `#[rudzio::test]` in a `#[cfg(any(test, rudzio_test))] mod tests`
/// gated lib module. The smallest crate that exercises cargo-rudzio's
/// `write_runner` pipeline end-to-end (aggregator generation,
/// build.rs, member bridging, proxy emission).
fn write_minimal_consumer(root: &Path, rudzio_root: &Path) -> io::Result<()> {
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

    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
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
         anyhow = { workspace = true }\n",
    )?;
    // `// rudzio_test` marker + `#[cfg(any(test, rudzio_test))]` is
    // the canonical broadened-cfg shape so cargo-rudzio compiles this
    // mod under its `--cfg rudzio_test` build.
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn trivial(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;

    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use std::process::Output;

    use super::{
        Command, Path, PathBuf, Stdio, TempDir, Test, fs, io, rudzio_source_root,
        shared_target_dir, write_minimal_consumer,
    };

    /// Spawn `cargo rudzio test` in `root` with `CARGO_TARGET_DIR=
    /// shared` set explicitly. Returns the spawned process's
    /// captured `Output`. Pipes both streams so the assertion site
    /// can grep stderr for the race signature.
    fn run_cargo_rudzio_with_shared_target(root: &Path, shared: &Path) -> io::Result<Output> {
        let cargo_rudzio_bin: &str = env!("CARGO_BIN_EXE_cargo-rudzio");
        Command::new(cargo_rudzio_bin)
            .arg("test")
            .current_dir(root)
            .env("CARGO_TARGET_DIR", shared)
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }

    /// Parallel-invocation contention reproducer. The 9-runtime
    /// matrix dispatches this test under 9 runtimes concurrently;
    /// each invocation spawns its own `cargo rudzio test` subprocess
    /// against its own synthetic consumer workspace, all sharing one
    /// `CARGO_TARGET_DIR`. Pre-fix at least one subprocess hits the
    /// `removing .../Cargo.toml: No such file or directory` race
    /// inside `generate::write_runner`. Post-fix each invocation
    /// writes to a distinct per-consumer aggregator subdir, no race.
    #[rudzio::test]
    async fn parallel_invocations_against_distinct_workspaces_sharing_target_dir_all_succeed(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new()?;
        let root = workspace.path();
        let rudzio_root: PathBuf = rudzio_source_root();
        let shared: PathBuf = shared_target_dir();
        fs::create_dir_all(&shared)?;
        write_minimal_consumer(root, &rudzio_root)?;

        let output = run_cargo_rudzio_with_shared_target(root, &shared)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

        let race_signature_present = stderr.contains("No such file or directory")
            && stderr.contains("rudzio-auto-runner");
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
