//! Proactive class-audit for the Bug #16 defect family: silent
//! disappearance of `#[rudzio::test]` surface into compile units
//! that the aggregator never links.
//!
//! # Invariant under test
//!
//! For any source file the rudzio toolchain could plausibly compile
//! under `--cfg rudzio_test`, a `#[rudzio::test]` declaration in
//! that file must satisfy one of the following two outcomes:
//!
//!  (a) it runs when `cargo rudzio test` is invoked against the
//!      workspace and appears in the captured stdout.
//!
//!  (b) `cargo rudzio test` exits non-zero AND emits a stderr
//!      message naming the offending file path so the user knows
//!      which test the toolchain refused to run.
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
//! | Location                                          | Bug #16 link                       |
//! |---------------------------------------------------|------------------------------------|
//! | `src/bin/<n>/main.rs` (entry file)                | adjacent — Bug #16 covers helper   |
//! | `examples/<n>.rs`                                  | audit candidate #4 (Examples)      |
//! | `benches/<n>.rs`                                   | audit candidate #4 (Benches)       |
//! | `tests/<sub>/helper.rs` (submodule of `tests/foo.rs`) | audit candidate #6 (helper gate) |
//! | `#[path = "../foo.rs"]` mod escaping `<member>/src/`  | audit candidate #1 (src/ walk)   |
//! | non-default `[lib].path = "src/<sub>/lib.rs"`      | not in audit — preventive          |
//!
//! Locations NOT covered here (separate defect class — different spec):
//!  * broken transitive path-dep metadata (silent-member-drop class —
//!    `extend_with_transitive_path_dep_members:1035` swallows
//!    `MetadataCommand::exec()` failures).
//!  * runtime-side scanned-vs-registered count invariant (requires
//!    codegen mechanism that doesn't exist yet — covered by the
//!    structural fix's integrity check, not a behavioural test).
//!
//! # Phase B migration
//!
//! Each `customize_*` helper starts from the suite-provided
//! `ctx.consumer_workspace()` (a per-test full-recursive copy of
//! the template) and mutates the consumer member to match the
//! fixture's target shape. The template's workspace `Cargo.toml`
//! already carries the rudzio path-dep + anyhow workspace deps so
//! the consumer manifest variants below reference them via
//! `{ workspace = true }`. The lib at `consumer/src/lib.rs` is
//! overwritten per-fixture to embed the fixture-specific
//! `lib_baseline_test` (the visibility floor — its absence catches
//! "the WHOLE run silently produced zero tests" without it being
//! confused for the regression assertion).

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// A trivial `src/lib.rs` carrying exactly ONE `#[rudzio::test]`.
/// The lib test is the visibility baseline: every fixture asserts
/// it ran (catching the failure mode "the WHOLE run silently
/// produced zero tests", which would otherwise pass a count check
/// against just the regression test).
fn write_lib_with_baseline_test(consumer_src: &Path) -> io::Result<()> {
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
fn customize_bin_main_test(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let consumer_src = consumer.join("src");
    let bin_dir = consumer_src.join("bin").join("server");
    fs::create_dir_all(&bin_dir)?;

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
         [[bin]]\n\
         name = \"server\"\n\
         path = \"src/bin/server/main.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    write_lib_with_baseline_test(&consumer_src)?;

    fs::write(
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
fn customize_example_test(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let consumer_src = consumer.join("src");
    let examples = consumer.join("examples");
    fs::create_dir_all(&examples)?;

    fs::write(
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

    fs::write(
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
fn customize_bench_test(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let consumer_src = consumer.join("src");
    let benches = consumer.join("benches");
    fs::create_dir_all(&benches)?;

    fs::write(
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

    fs::write(
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

/// Fixture: `tests/foo.rs` declares `mod helper;`; cargo resolves
/// the helper at `tests/foo/helper.rs`; the helper file contains
/// the `#[rudzio::test]`. The audit (#6) flagged
/// `discover_test_root_helpers` as gating subdirectory walks on
/// the entry file being named `mod.rs` / `main.rs` / `lib.rs` —
/// a flat `tests/foo.rs` entry skips its `<sub>/` mod sources.
/// The marker should still appear in the dispatcher (cargo
/// compiles `tests/foo.rs` + its `mod helper;` as one test binary
/// regardless) but the proxy + helper-mount pipeline rudzio
/// generates may drop the helper.
fn customize_tests_subdir_helper(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let consumer_src = consumer.join("src");
    let tests = consumer.join("tests");
    let foo_subdir = tests.join("foo");
    fs::create_dir_all(&foo_subdir)?;

    // Explicit `[[test]]` entry rather than relying on
    // `autotests = true` discovery. Cargo's auto-discovery would
    // pick `tests/foo.rs` up identically; the explicit form just
    // pins the test name and removes one variable from the
    // fixture's interpretation.
    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[test]]\n\
         name = \"foo\"\n\
         path = \"tests/foo.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    write_lib_with_baseline_test(&consumer_src)?;

    // The test-binary entry file. Declares the helper submodule;
    // cargo's mod resolution finds it at `tests/foo/helper.rs`.
    // `#[rudzio::main]` is required because this test binary is
    // declared `harness = false`.
    fs::write(
        tests.join("foo.rs"),
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n\
         \n\
         mod helper;\n",
    )?;

    // The marker — a rudzio test living in a submodule of the
    // test binary, NOT in the entry file. Whether rudzio's
    // helper-discovery machinery walks here is the question
    // under test.
    fs::write(
        foo_subdir.join("helper.rs"),
        rudzio_test_block("tests_subdir_helper_test"),
    )?;

    Ok(())
}

/// Fixture: `src/lib.rs` uses `#[path = "../shared/escaped.rs"]`
/// to pull a file from OUTSIDE the member's `src/` tree into the
/// lib's module graph. Cargo compiles it (it's reachable from
/// lib.rs); rudzio's `detect_src_rudzio_suite` walks only `src/`,
/// so it can't see the marker at scan time. Audit candidate #1:
/// the canonical "scanner walks A, code lives at B" mismatch.
fn customize_path_attr_escape(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let consumer_src = consumer.join("src");
    let shared = workspace.join("shared");
    fs::create_dir_all(&shared)?;

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

    // src/lib.rs: baseline test in place + `#[path]` mod pointing
    // outside src/. The `// rudzio_test` marker comment keeps
    // `detect_src_rudzio_suite` returning true so the member gets
    // bridged at all; without it the bridge wouldn't exist and
    // both tests would silently disappear (different failure
    // class, not what this fixture is testing).
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
         \x20\x20\x20\x20\x20\x20\x20\x20fn lib_baseline_test(_t: &super::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         #[path = \"../../shared/escaped.rs\"]\n\
         mod escaped;\n",
    )?;

    // shared/escaped.rs — outside the member's manifest dir.
    // Reachable from lib.rs via `#[path]` but invisible to any
    // scanner that walks `<member>/src/`.
    fs::write(
        shared.join("escaped.rs"),
        rudzio_test_block("path_attr_escape_test"),
    )?;

    Ok(())
}

/// Fixture: member declares `[lib].path = "src/custom/lib.rs"`,
/// not the default `src/lib.rs`. cargo metadata reports the
/// custom path in `target.src_path`. The question: does rudzio's
/// `locate_src_lib_path` (1128) honour it, and do all downstream
/// scanners walk the right directory?
fn customize_custom_lib_path(workspace: &Path) -> io::Result<()> {
    let consumer = workspace.join("consumer");
    let custom_dir = consumer.join("src").join("custom");
    fs::create_dir_all(&custom_dir)?;

    // Remove the template's default `src/lib.rs` so the member
    // truly has only the custom-path lib — otherwise cargo would
    // see two libs and complain.
    let default_lib = consumer.join("src").join("lib.rs");
    if default_lib.exists() {
        fs::remove_file(&default_lib)?;
    }

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
         path = \"src/custom/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )?;

    // The lib at the custom path holds both the baseline and the
    // marker. Since there's no `src/lib.rs`, if rudzio fails to
    // honour `[lib].path` BOTH tests vanish — the assertion
    // catches that via the baseline-presence pre-check.
    fs::write(
        custom_dir.join("lib.rs"),
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
         \n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn custom_lib_path_test(_t: &super::Test) -> anyhow::Result<()> {\n\
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
        Command, IsolatedCargoTest, Output, Path, Stdio, customize_bench_test,
        customize_bin_main_test, customize_custom_lib_path, customize_example_test,
        customize_path_attr_escape, customize_tests_subdir_helper, io,
    };

    /// Outcome categories the invariant accepts. `SilentlyDisappeared`
    /// is the bug-class symptom and the only rejection state for
    /// these tests.
    enum InvariantOutcome {
        /// Marker test was explicitly refused: non-zero exit AND
        /// a stderr message naming the file path. Acceptable per
        /// the "zero silent failure" rule even if the structural
        /// fix doesn't make the location work.
        LoudlyRefused,
        /// Marker test silently disappeared: cargo rudzio test
        /// exited 0, summary doesn't include the marker, no stderr
        /// message names the offending file. **This is the bug.**
        SilentlyDisappeared,
        /// Marker test ran and was counted in the summary.
        Visible,
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

    /// Single-place reporter for the `SilentlyDisappeared` verdict
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
        let workspace: &Path = ctx.consumer_workspace();
        customize_bin_main_test(workspace)?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
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
            InvariantOutcome::SilentlyDisappeared => Err(silent_disappearance_failure(
                marker_path,
                marker_name,
                &output,
            )),
        }
    }

    /// A `#[rudzio::test]` inside `examples/<n>.rs` must satisfy
    /// the surface-visibility invariant. The audit identified
    /// `examples/` as never bridged; this test pins the contract.
    #[rudzio::test]
    async fn rudzio_test_in_example_file_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_example_test(workspace)?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
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
            InvariantOutcome::SilentlyDisappeared => Err(silent_disappearance_failure(
                marker_path,
                marker_name,
                &output,
            )),
        }
    }

    /// A `#[rudzio::test]` inside `benches/<n>.rs` must satisfy
    /// the surface-visibility invariant. The audit identified
    /// `benches/` as never bridged.
    #[rudzio::test]
    async fn rudzio_test_in_bench_file_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_bench_test(workspace)?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
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
            InvariantOutcome::SilentlyDisappeared => Err(silent_disappearance_failure(
                marker_path,
                marker_name,
                &output,
            )),
        }
    }

    /// A `#[rudzio::test]` inside `tests/foo/helper.rs` (a
    /// submodule of `tests/foo.rs`) must satisfy the invariant.
    /// Audit candidate #6: `discover_test_root_helpers` requires
    /// the entry file to be `mod.rs` / `main.rs` / `lib.rs` —
    /// a flat `tests/foo.rs` entry skips its `<sub>/` mod sources.
    #[rudzio::test]
    async fn rudzio_test_in_tests_subdir_helper_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_tests_subdir_helper(workspace)?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let marker_path = "tests/foo/helper.rs";
        let marker_name = "tests_subdir_helper_test";

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
            InvariantOutcome::SilentlyDisappeared => Err(silent_disappearance_failure(
                marker_path,
                marker_name,
                &output,
            )),
        }
    }

    /// A `#[rudzio::test]` reached via `#[path = "../shared/escaped.rs"]`
    /// from `src/lib.rs` must satisfy the invariant. Audit
    /// candidate #1: `detect_src_rudzio_suite` walks only
    /// `<member>/src/`, so a marker in a path-escaped file is
    /// invisible at scan time. The bridge may also fail to
    /// re-resolve the `#[path]` relative reference under its
    /// own `<bridge>/src/lib.rs` location.
    #[rudzio::test]
    async fn rudzio_test_in_path_attr_escape_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_path_attr_escape(workspace)?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let marker_path = "shared/escaped.rs";
        let marker_name = "path_attr_escape_test";

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
            InvariantOutcome::SilentlyDisappeared => Err(silent_disappearance_failure(
                marker_path,
                marker_name,
                &output,
            )),
        }
    }

    /// A member declaring `[lib].path = "src/custom/lib.rs"` (NOT
    /// the default `src/lib.rs`) must have its lib's
    /// `#[rudzio::test]`s reach the dispatcher. Not in the audit
    /// directly — preventive: `cargo metadata` reports the custom
    /// `src_path` so rudzio's `locate_src_lib_path` should pick
    /// it up; this test confirms every downstream scanner walks
    /// the resolved directory rather than hardcoding `src/`.
    #[rudzio::test]
    async fn rudzio_test_in_custom_lib_path_is_not_silently_invisible(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace: &Path = ctx.consumer_workspace();
        customize_custom_lib_path(workspace)?;

        let output = run_cargo_rudzio_test(workspace, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let marker_path = "src/custom/lib.rs";
        let marker_name = "custom_lib_path_test";

        // Baseline check is special-cased here: the baseline test
        // lives in the SAME file as the marker (both inside the
        // non-default lib path), so a failure of the baseline
        // here means rudzio's `[lib].path` handling is broken,
        // not the fixture. The classifier still distinguishes
        // visible / loud / silent so the failure mode is precise.
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::ensure!(
            stdout.contains("lib_baseline_test") || !output.status.success(),
            "lib baseline test is missing from successful output — either \
             rudzio's `[lib].path` handling is broken (then the marker \
             assertion below will also fail loudly), or the fixture is \
             broken; either way investigate before continuing.\n\
             --- captured stdout ---\n{stdout}\n\
             --- captured stderr ---\n{stderr}",
            stderr = String::from_utf8_lossy(&output.stderr),
        );

        match classify(&output, marker_name, marker_path) {
            InvariantOutcome::Visible | InvariantOutcome::LoudlyRefused => Ok(()),
            InvariantOutcome::SilentlyDisappeared => Err(silent_disappearance_failure(
                marker_path,
                marker_name,
                &output,
            )),
        }
    }
}
