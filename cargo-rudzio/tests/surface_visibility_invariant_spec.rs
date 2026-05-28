//! Proactive class-audit for the Bug #16 defect family: silent
//! disappearance of `#[rudzio::test]` surface into compile units
//! that the aggregator never links.
//!
//! # Invariant under test
//!
//! For any source file the rudzio toolchain could plausibly compile
//! under `--cfg rudzio_test`, a `#[rudzio::test]` declaration in
//! that file must EITHER
//!
//!  (a) run when `cargo rudzio test` is invoked against the
//!      workspace and appear in the captured stdout, OR
//!
//!  (b) cause `cargo rudzio test` to exit non-zero AND emit a
//!      stderr message naming the offending file path so the user
//!      knows which test the toolchain refused to run.
//!
//! Silent green runs with disappeared tests are FORBIDDEN.
//!
//! # Why these tests exist before any fix
//!
//! Bug #16 manifested as a green run with a wrong test count. The
//! underlying defect class — information loss at scan boundary,
//! asymmetric scopes between detection and emission, silent
//! fallthrough on cases nobody modelled — almost certainly hides
//! more instances of the same shape. These tests pin the invariant
//! BEFORE any fix lands so:
//!
//!  * any structural fix that makes a location work end-to-end
//!    satisfies the (a) clause and turns the test green
//!  * any deliberate refusal that hard-errors with a clear message
//!    satisfies the (b) clause and turns the test green
//!  * the silent-fallthrough state (current pre-fix) is the only
//!    outcome that keeps the test red
//!
//! Each test exercises one source-tree shape. None of them care
//! whether the eventual fix supports the location structurally or
//! rejects it explicitly — only that the silent-fallthrough case
//! is impossible.
//!
//! # Coverage map
//!
//! | Location                              | Bug #16 link                          |
//! |---------------------------------------|---------------------------------------|
//! | `src/bin/<n>/main.rs` (entry)         | adjacent — Bug #16 covers helper file |
//! | `examples/<n>.rs`                     | audit candidate #4 (Examples ignored) |
//! | `benches/<n>.rs`                      | audit candidate #4 (Benches ignored)  |
//!
//! Locations NOT covered here (left for follow-up specs):
//!  * `#[path = "../foo.rs"]` mod escaping `<member>/src/`
//!  * non-default `[lib].path = "src/somewhere/else.rs"`
//!  * `tests/<sub>/main.rs` with `autotests = false`
//!  * broken transitive path-dep metadata (silent-member-drop class)
//!  * runtime-side scanned-vs-registered count invariant

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;
use tempfile::TempDir;

/// Workspace root of the in-tree `rudzio` crate. The fixtures'
/// `[workspace.dependencies] rudzio = { path = <this> }` makes each
/// synthetic consumer exercise the rudzio code under test.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Common workspace `Cargo.toml` head. Single `consumer` member,
/// pinned rudzio path-dep + anyhow, edition 2024.
fn write_workspace_manifest(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    std::fs::write(
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
    )
}

/// A trivial `src/lib.rs` carrying exactly ONE `#[rudzio::test]`.
/// The lib test is the visibility baseline: every fixture asserts
/// it ran (catching the failure mode "the WHOLE run silently
/// produced zero tests", which would otherwise pass a count check
/// against just the regression test).
fn write_lib_with_baseline_test(consumer_src: &Path) -> io::Result<()> {
    std::fs::write(
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
         \x20\x20\x20\x20\x20\x20\x20\x20fn lib_baseline_test(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )
}

/// Canonical broadened-cfg test body. `marker_name` becomes the
/// fn name; assertions later substring-grep for it in the captured
/// stdout to confirm the dispatcher saw the registration.
fn rudzio_test_block(marker_name: &str) -> String {
    format!(
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {{\n\
         \x20\x20\x20\x20use rudzio::common::context::{{Suite, Test}};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn {marker_name}(_t: &super::Test) -> anyhow::Result<()> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}}\n\
         \x20\x20\x20\x20}}\n\
         }}\n",
    )
}

/// Fixture: `src/bin/server/main.rs` contains a `#[rudzio::test]`
/// inside an `#[cfg(any(test, rudzio_test))]` block alongside the
/// bin's `fn main()`. The variant Bug #16 covers
/// (`src/bin/server/ops.rs` — a submodule of the bin) is treated
/// by the rudzio toolchain identically as far as the lib-vs-bin
/// compile-unit split goes; this variant pins the entry file
/// specifically because some structural fixes might handle
/// `mod ops;`-style submodules differently from `main.rs` itself.
fn write_fixture_bin_main_test(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    write_workspace_manifest(root, rudzio_root)?;
    let consumer = root.join("consumer");
    let consumer_src = consumer.join("src");
    let bin_dir = consumer_src.join("bin").join("server");
    std::fs::create_dir_all(&bin_dir)?;

    std::fs::write(
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
         [[bin]]\n\
         name = \"server\"\n\
         path = \"src/bin/server/main.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    write_lib_with_baseline_test(&consumer_src)?;

    // `main.rs` carries BOTH the `fn main()` (required for the
    // bin to link) AND the rudzio test block. Under `cargo build`
    // the bin compiles as `consumer:bin:server`; under
    // `cargo rudzio test` the bridge declares `[[bin]] path = ...`
    // pointing at this same file, so the file compiles inside the
    // bridge — but linkme aggregation only reaches the bridge
    // LIB, not the bin compile unit.
    std::fs::write(
        bin_dir.join("main.rs"),
        format!(
            "{block}\n\
             fn main() {{\n\
             \x20\x20\x20\x20println!(\"server stub\");\n\
             }}\n",
            block = rudzio_test_block("bin_entry_main_rs_test"),
        ),
    )?;

    Ok(())
}

/// Fixture: `examples/sample.rs` contains a `#[rudzio::test]`
/// alongside a trivial `fn main()`. `examples/` is one of cargo's
/// standard target kinds; cargo can build them under `cargo build
/// --examples`. Rudzio's CLI consumes `--examples` but doesn't
/// honour it; what about a `#[rudzio::test]` in an example file
/// during a plain `cargo rudzio test` invocation?
fn write_fixture_example_test(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    write_workspace_manifest(root, rudzio_root)?;
    let consumer = root.join("consumer");
    let consumer_src = consumer.join("src");
    let examples = consumer.join("examples");
    std::fs::create_dir_all(&consumer_src)?;
    std::fs::create_dir_all(&examples)?;

    std::fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         autoexamples = true\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    write_lib_with_baseline_test(&consumer_src)?;

    std::fs::write(
        examples.join("sample.rs"),
        format!(
            "{block}\n\
             fn main() {{\n\
             \x20\x20\x20\x20println!(\"example stub\");\n\
             }}\n",
            block = rudzio_test_block("example_file_test"),
        ),
    )?;

    Ok(())
}

/// Fixture: `benches/sample.rs` contains a `#[rudzio::test]`. Same
/// shape as the examples fixture but under `benches/`. Cargo's
/// bench target kind has stricter compile rules (the `test` crate
/// harness is involved), but a `#[rudzio::test]` declaration
/// inside an `#[cfg(any(test, rudzio_test))]` mod should be
/// equally subject to the "is this compile unit linked into the
/// aggregator?" invariant.
fn write_fixture_bench_test(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    write_workspace_manifest(root, rudzio_root)?;
    let consumer = root.join("consumer");
    let consumer_src = consumer.join("src");
    let benches = consumer.join("benches");
    std::fs::create_dir_all(&consumer_src)?;
    std::fs::create_dir_all(&benches)?;

    std::fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         autobenches = true\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bench]]\n\
         name = \"sample\"\n\
         path = \"benches/sample.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    write_lib_with_baseline_test(&consumer_src)?;

    std::fs::write(
        benches.join("sample.rs"),
        format!(
            "{block}\n\
             fn main() {{\n\
             \x20\x20\x20\x20println!(\"bench stub\");\n\
             }}\n",
            block = rudzio_test_block("bench_file_test"),
        ),
    )?;

    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        Command, IsolatedCargoTest, Output, Path, PathBuf, Stdio, TempDir, io, rudzio_source_root,
        write_fixture_bench_test, write_fixture_bin_main_test, write_fixture_example_test,
    };

    /// Outcome categories the invariant accepts. The third (silent
    /// disappearance) is the bug-class symptom and the only
    /// rejection state for these tests.
    enum InvariantOutcome {
        /// Marker test ran and was counted in the summary.
        Visible,
        /// Marker test silently disappeared: cargo rudzio test
        /// exited 0, summary doesn't include the marker, no stderr
        /// message names the offending file. **This is the bug.**
        SilentlyDisappeared,
        /// Marker test was explicitly refused: non-zero exit AND
        /// a stderr message naming the file path. Acceptable per
        /// the "zero silent failure" rule even if the structural
        /// fix doesn't make the location work.
        LoudlyRefused,
    }

    /// Classify the spawned `cargo rudzio test` invocation against
    /// the invariant. `marker_name` is the test fn name the
    /// fixture put inside the suspect location; `marker_path` is
    /// the relative path of the file inside the consumer member
    /// (e.g. `src/bin/server/main.rs`, `examples/sample.rs`).
    fn classify(output: &Output, marker_name: &str, marker_path: &str) -> InvariantOutcome {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let marker_ran = stdout.contains(marker_name);

        if output.status.success() && marker_ran {
            return InvariantOutcome::Visible;
        }
        if !output.status.success() {
            // Loud refusal demands either the path or the marker
            // name in stderr — something that lets the user
            // identify which test the toolchain rejected.
            let file_named = stderr.contains(marker_path) || stderr.contains(marker_name);
            if file_named {
                return InvariantOutcome::LoudlyRefused;
            }
        }
        InvariantOutcome::SilentlyDisappeared
    }

    /// Spawn `cargo-rudzio test` against the fixture with per-test
    /// isolated cargo state (hardlinked registry, fresh target
    /// dir). `CARGO_NET_OFFLINE=true` keeps the spawned cargo away
    /// from the network; the pre-warmed registry covers every
    /// crate the fixture pulls in.
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

    /// Single-place reporter for the SilentlyDisappeared verdict
    /// so each test site stays focused on the location-specific
    /// scaffolding.
    fn silent_disappearance_failure(
        marker_path: &str,
        marker_name: &str,
        output: &Output,
    ) -> anyhow::Error {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::anyhow!(
            "surface-visibility invariant violated for `{marker_path}`:\n\
             `cargo rudzio test` exited successfully but `{marker_name}` is \
             absent from the captured output AND no stderr message names \
             the file. This is the Bug #16 defect class: a rudzio surface \
             silently disappeared into a compile unit the aggregator does \
             not link. ANY structural fix or explicit loud refusal \
             resolves this; only the silent-fallthrough state fails the \
             invariant.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
        )
    }

    /// A `#[rudzio::test]` inside `src/bin/<n>/main.rs` — the bin's
    /// entry file — must satisfy the surface-visibility invariant.
    /// Variant of Bug #16: Bug #16's spec covers a bin SUBMODULE
    /// (`src/bin/server/ops.rs`); this one covers the bin entry
    /// itself because some fixes may treat the entry file
    /// differently from `mod ops;`-style submodules.
    #[rudzio::test]
    async fn rudzio_test_in_bin_main_rs_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        let rudzio_root: PathBuf = rudzio_source_root();
        write_fixture_bin_main_test(root, &rudzio_root)?;

        let output = run_cargo_rudzio_test(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let marker_path = "src/bin/server/main.rs";
        let marker_name = "bin_entry_main_rs_test";

        // Baseline-first: the lib test MUST run; if it didn't,
        // the fixture itself is broken and the regression
        // assertion is meaningless.
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::ensure!(
            stdout.contains("lib_baseline_test") || !output.status.success(),
            "lib baseline test is missing from successful output — the \
             fixture is broken, investigate before treating any downstream \
             assertion as meaningful.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
            stderr = String::from_utf8_lossy(&output.stderr),
        );

        match classify(&output, marker_name, marker_path) {
            InvariantOutcome::Visible | InvariantOutcome::LoudlyRefused => Ok(()),
            InvariantOutcome::SilentlyDisappeared => {
                Err(silent_disappearance_failure(marker_path, marker_name, &output))
            }
        }
    }

    /// A `#[rudzio::test]` inside `examples/<n>.rs` must satisfy
    /// the surface-visibility invariant. The audit identified
    /// `examples/` as never bridged; this test pins the contract.
    #[rudzio::test]
    async fn rudzio_test_in_example_file_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        let rudzio_root: PathBuf = rudzio_source_root();
        write_fixture_example_test(root, &rudzio_root)?;

        let output = run_cargo_rudzio_test(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let marker_path = "examples/sample.rs";
        let marker_name = "example_file_test";

        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::ensure!(
            stdout.contains("lib_baseline_test") || !output.status.success(),
            "lib baseline test is missing from successful output — the \
             fixture is broken, investigate before treating any downstream \
             assertion as meaningful.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
            stderr = String::from_utf8_lossy(&output.stderr),
        );

        match classify(&output, marker_name, marker_path) {
            InvariantOutcome::Visible | InvariantOutcome::LoudlyRefused => Ok(()),
            InvariantOutcome::SilentlyDisappeared => {
                Err(silent_disappearance_failure(marker_path, marker_name, &output))
            }
        }
    }

    /// A `#[rudzio::test]` inside `benches/<n>.rs` must satisfy
    /// the surface-visibility invariant. The audit identified
    /// `benches/` as never bridged.
    #[rudzio::test]
    async fn rudzio_test_in_bench_file_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        let rudzio_root: PathBuf = rudzio_source_root();
        write_fixture_bench_test(root, &rudzio_root)?;

        let output = run_cargo_rudzio_test(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let marker_path = "benches/sample.rs";
        let marker_name = "bench_file_test";

        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::ensure!(
            stdout.contains("lib_baseline_test") || !output.status.success(),
            "lib baseline test is missing from successful output — the \
             fixture is broken, investigate before treating any downstream \
             assertion as meaningful.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
            stderr = String::from_utf8_lossy(&output.stderr),
        );

        match classify(&output, marker_name, marker_path) {
            InvariantOutcome::Visible | InvariantOutcome::LoudlyRefused => Ok(()),
            InvariantOutcome::SilentlyDisappeared => {
                Err(silent_disappearance_failure(marker_path, marker_name, &output))
            }
        }
    }
}
