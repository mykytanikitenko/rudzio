//! Structural regression guard for Bug #18: `cargo-rudzio` rebuilds
//! must produce a distinct aggregator subdir name so cargo's
//! fingerprinter cannot reuse a stale aggregator binary from a
//! previous `cargo-rudzio` version.
//!
//! # Symptom (pre-fix)
//!
//! `<aggregator>/target/` is preserved across `cargo rudzio test`
//! invocations. The aggregator binary at
//! `<aggregator>/target/debug/rudzio-auto-runner` is keyed by
//! cargo's fingerprinter on source-content hashes — none of which
//! include `cargo-rudzio`'s binary version. When `cargo-rudzio` is
//! rebuilt and generates byte-identical aggregator source for an
//! unchanged consumer workspace, cargo skips the relink and the
//! OLD aggregator binary persists with OLD runner code.
//! Behavioural fixes shipped in `cargo-rudzio` silently fail to
//! take effect.
//!
//! # Cause sketch (informs assertion)
//!
//! Pre-fix `aggregator_subdir_name` (`cargo-rudzio/src/generate.rs`)
//! hashed only `workspace_root` into the subdir name. Two
//! invocations against the same workspace from different
//! `cargo-rudzio` versions produced the same path and shared the
//! same `target/` directory, allowing the fingerprinter to skip
//! the relink.
//!
//! The Phase 0b fix hashes `BUILD_STAMP` (captured at compile
//! time by `cargo-rudzio/build.rs` as
//! `<CARGO_PKG_VERSION>-<unix_epoch_seconds>`) into the subdir
//! name alongside the workspace hash. New format:
//! `rudzio-auto-runner-<wshash>-<stamphash>`. Different
//! `cargo-rudzio` builds → different stamps → different subdirs →
//! cargo cannot reuse the stale binary.
//!
//! # Assertion
//!
//! Structural: walk the consumer's `CARGO_TARGET_DIR`, locate the
//! single `rudzio-auto-runner-*` directory, parse its name as
//! `rudzio-auto-runner-<wshash>-<stamphash>`, and confirm:
//!   1. The subdir name conforms to the `<NAME>-<wshash>-<stamphash>`
//!      layout (i.e. `rsplitn(3, '-')` leaves `rudzio-auto-runner`
//!      intact as the last segment).
//!   2. The stamp-hash component is 16 ASCII hex chars (a `u64`
//!      formatted as `{:016x}`).
//!   3. The observed stamp-hash differs from a synthetic
//!      alternative computed from a hypothetical stamp string
//!      (`"0.0.0-99999"`). A regression that drops the stamp from
//!      the subdir name would make the observed hash collapse into
//!      something else (or fail the layout check entirely).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;
use tempfile::TempDir;

/// FNV-1a 64-bit hash — must mirror the algorithm in
/// `cargo-rudzio/src/generate.rs::Plan::aggregator_subdir_name`.
/// Used to compute a SYNTHETIC stamp-hash candidate the assertion
/// can compare against the observed value. A regression that
/// silently changes the hash function would still surface as a
/// layout failure first; this helper is purely the "alternative
/// stamp produces alternative hash" probe.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_64: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME_64: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(FNV_OFFSET_64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME_64)
    })
}

/// Workspace root of the in-tree `rudzio` crate. The fixture's
/// `[workspace.dependencies] rudzio = { path = <this> }` makes the
/// synthetic consumer exercise the rudzio code under test, not a
/// published version.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a minimal lib-only consumer at `root`:
///
/// ```text
/// root/
/// ├── Cargo.toml          (workspace)
/// └── consumer/
///     ├── Cargo.toml      ([lib] only)
///     └── src/
///         └── lib.rs      (1 #[rudzio::test] — trivial_test)
/// ```
///
/// The single test gates the aggregator subdir creation: without
/// at least one rudzio surface in the workspace, `cargo rudzio
/// test` would emit an empty aggregator and the structural probe
/// below would find nothing to inspect.
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

    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join("src"))?;

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
         anyhow = { workspace = true }\n",
    )?;

    fs::write(
        consumer.join("src").join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn trivial_test(_t: &super::Test) -> anyhow::Result<()> {\n\
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
        Command, IsolatedCargoTest, Output, Path, PathBuf, Stdio, TempDir, fnv1a64, fs, io,
        rudzio_source_root, write_fixture,
    };

    /// **Bug #18 structural regression guard**: the aggregator
    /// subdir name must encode both the workspace hash AND the
    /// `cargo-rudzio` build stamp so cargo's fingerprinter cannot
    /// reuse a stale aggregator binary across `cargo-rudzio`
    /// rebuilds.
    ///
    /// The assertion is structural (parses the directory name)
    /// rather than behavioural (running `cargo-rudzio` twice with
    /// different binaries inside one test would require building
    /// two `cargo-rudzio` binaries side-by-side, which the
    /// integration-test harness does not provide). A future
    /// behavioural test could exercise the full path by stubbing
    /// the runner binary.
    #[rudzio::test]
    async fn cargo_rudzio_test_creates_distinct_aggregator_subdir_when_stamp_changes(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        let rudzio_root: PathBuf = rudzio_source_root();
        write_fixture(root, &rudzio_root)?;

        let output = run_cargo_rudzio_test(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        anyhow::ensure!(
            output.status.success(),
            "`cargo rudzio test` exited {:?}; expected success.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
            output.status.code(),
        );

        let observed_path = find_aggregator_subdir(ctx.cargo_target_dir())?;
        let observed_name = observed_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!("subdir has no valid UTF-8 name: {}", observed_path.display())
            })?;

        // Format: `rudzio-auto-runner-<wshash>-<stamphash>`.
        // `AGGREGATOR_NAME` itself contains hyphens; `rsplitn(3,
        // '-')` peels exactly the two trailing hash components and
        // leaves the `AGGREGATOR_NAME` intact as the last segment.
        // If the last segment is anything other than
        // `rudzio-auto-runner`, the subdir name layout regressed.
        // Slice pattern instead of indexing because the workspace
        // forbids `indexing_slicing`.
        let parts: Vec<&str> = observed_name.rsplitn(3, '-').collect();
        let (stamp_hash_observed, aggregator_name_segment) = match parts.as_slice() {
            [stamp, _workspace, aggregator] => (*stamp, *aggregator),
            other => anyhow::bail!(
                "Expected exactly 3 segments via rsplitn(3, '-'); got {} from {observed_name:?}.",
                other.len(),
            ),
        };
        anyhow::ensure!(
            aggregator_name_segment == "rudzio-auto-runner",
            "Expected AGGREGATOR_NAME suffix to be `rudzio-auto-runner`; got \
             {aggregator_name_segment:?} (full name: {observed_name:?}). Bug #18 \
             regression: the subdir name no longer follows the \
             `<NAME>-<wshash>-<stamphash>` layout — the stamp hash component has \
             been dropped or fused with the workspace hash.",
        );
        anyhow::ensure!(
            stamp_hash_observed.len() == 16,
            "Expected stamp hash to be 16 hex chars (a u64 as {{:016x}}); got {} chars: {stamp_hash_observed:?}",
            stamp_hash_observed.len(),
        );
        anyhow::ensure!(
            stamp_hash_observed
                .chars()
                .all(|character| character.is_ascii_hexdigit()),
            "Expected stamp hash to be all ASCII hex digits; got {stamp_hash_observed:?}",
        );

        let synthetic_stamp_hash = format!("{:016x}", fnv1a64(b"0.0.0-99999"));
        anyhow::ensure!(
            stamp_hash_observed != synthetic_stamp_hash,
            "Observed stamp hash {stamp_hash_observed:?} matches the synthetic \
             alternative {synthetic_stamp_hash:?}. Either the FNV implementation \
             diverged between the spec and production code, or the fixture's stamp \
             accidentally equals the synthetic value.",
        );

        Ok(())
    }

    /// Locate the unique `rudzio-auto-runner-*` directory under
    /// `target_dir`. Errors if zero or more-than-one match exist.
    fn find_aggregator_subdir(target_dir: &Path) -> anyhow::Result<PathBuf> {
        let matches: Vec<PathBuf> = fs::read_dir(target_dir)?
            .filter_map(io::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rudzio-auto-runner-"))
            })
            .collect();
        anyhow::ensure!(
            matches.len() == 1,
            "Expected exactly 1 aggregator subdir under {}; found {}: {:?}",
            target_dir.display(),
            matches.len(),
            matches,
        );
        let mut iter = matches.into_iter();
        let path = iter
            .next()
            .ok_or_else(|| anyhow::anyhow!("matches.len() == 1 was just asserted"))?;
        Ok(path)
    }

    /// Spawn `cargo-rudzio test` against the fixture under per-test
    /// isolated cargo state (hardlinked registry + fresh target
    /// dir). Mirrors the helper in
    /// `bug_16_bin_source_tests_invisible_spec.rs`.
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
}
