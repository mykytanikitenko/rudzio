//! Proactive edge-case e2e coverage for cargo-rudzio's **test-target
//! discovery** and **helper-module hoisting**.
//!
//! Bugs #8 (bin-only integration tests never registered) and #10
//! (`mod harness;` declared only in a `tests/main.rs` shim never mounted)
//! lived in this exact corner of `cargo-rudzio/src/generate.rs`. This spec
//! exercises the periphery of that code against **operator-shaped**
//! consumer workspaces — real `members = ["crates/*"]` resolver-2 workspaces
//! with a `file://` git dependency on the rudzio repo under test, not
//! synthetic path-dep toy fixtures (which hid the original bugs).
//!
//! # Fixture contract (shared by every case)
//!
//! * workspace manifest: `members = ["crates/*"]`, `resolver = "2"`,
//!   `[workspace.package] edition = "2024"`, `rudzio` as a `file://` git
//!   dep pinned to the checkout HEAD (inherited via `rudzio.workspace =
//!   true`), `[workspace.lints.rust] warnings = "deny"` +
//!   `unsafe_code = "deny"` + `unexpected_cfgs` check-cfg `cfg(rudzio_test)`,
//!   `panic = "abort"` in dev + release, and a `.cargo/config.toml` with
//!   `build-dir = "tmp/build-dir"`.
//! * each member drives `rudzio::runtime::futures::ThreadPool` in its
//!   `#[rudzio::suite]` (the `runtime-futures` feature the git dep enables).
//!
//! # Assertion strategy
//!
//! Every case runs `cargo rudzio test` against its fixture and asserts
//! **exit 0 AND the exact set of passed test names** (parsed from the
//! runner's `[OK]` result lines, skipping `setup`/`teardown` lifecycle
//! lines) **AND the `N passed` summary count**. Case 9 (nested
//! `mod inner;` holding a `#[rudzio::suite]`) is a surface-visibility
//! invariant instead: the inner test must either run or the tool must
//! refuse loudly — a silent drop is never acceptable.
//!
//! # Coverage map
//!
//! | # | Case |
//! |---|------|
//! | 1 | lib, shim declares a single directory helper (`mod harness;`) |
//! | 2 | lib, shim declares two directory helpers (`harness` + `fixtures`) |
//! | 3 | lib, helper carries a non-Rust `data/sample.bin` read via `include_bytes!` |
//! | 4 | lib, flat `tests/other.rs` root (no shim) declares `mod helper;` |
//! | 5 | lib, shim declares two sibling test files (exact set of 2) |
//! | 6 | lib, helper `harness/mod.rs` declares a nested `pub mod fixtures;` |
//! | 7 | bin-only, shim declares a test module AND a helper (`CARGO_BIN_EXE_*`) |
//! | 8 | lib+bin+`src/bin/extra.rs`, integration test runs `CARGO_BIN_EXE_extra` |
//! | 9 | shim `mod suite_a;` → `suite_a/inner.rs` holds the suite (no silent drop) |
//! | 10 | `autotests = true`, `tests/a.rs` declares `mod common;` (`common/mod.rs`) |

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, RunSummary, assert_exact_test_names,
    assert_fresh_binary, assert_summary, combined_output, current_git_head, repo_root,
};
use rudzio::runtime::tokio::Multithread;

/// The `[[test]] name = "main" path = "tests/main.rs" harness = false`
/// shim entry every shim-based case declares.
const MAIN_SHIM_TEST_ENTRY: &str = "[[test]]\n\
     name = \"main\"\n\
     path = \"tests/main.rs\"\n\
     harness = false\n";

/// Outcome categories for the surface-visibility invariant (case 9).
enum InvariantOutcome {
    /// The tool refused loudly: non-zero exit AND a stderr message naming
    /// the offending file or marker.
    LoudlyRefused,
    /// The marker test silently disappeared: exit 0, absent from stdout,
    /// no stderr message names the file. This is the bug.
    SilentlyDisappeared,
    /// The marker test ran and was counted.
    Visible,
}

/// The checkout's real HEAD SHA, used as the `rev` pin so the git
/// dependency builds the rudzio code under test rather than a published
/// version.
fn rudzio_git_rev() -> anyhow::Result<String> {
    current_git_head()
        .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))
}

/// The absolute `file://` git URL for the rudzio repo this test lives in.
fn rudzio_git_url() -> String {
    format!("file://{}", repo_root().display())
}

/// Write the shared operator-shaped workspace manifest + `.cargo/config.toml`.
///
/// `rev` is the checkout HEAD SHA used as the git-dep pin. Every case
/// calls this once to materialise the workspace root, then writes its own
/// member(s) under `root/crates/`.
fn write_workspace_manifest(root: &Path, rev: &str) -> io::Result<()> {
    fs::create_dir_all(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo").join("config.toml"),
        "[build]\nbuild-dir = \"tmp/build-dir\"\n",
    )?;
    let git_url = rudzio_git_url();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"crates/*\"]\n\
             resolver = \"2\"\n\
             \n\
             [workspace.dependencies]\n\
             anyhow = \"=1.0.104\"\n\
             rudzio = {{ git = \"{git_url}\", rev = \"{rev}\", features = [\"runtime-futures\", \"runtime-compio\", \"common\"] }}\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.lints.rust]\n\
             warnings = \"deny\"\n\
             unsafe_code = \"deny\"\n\
             unexpected_cfgs = {{ level = \"warn\", check-cfg = ['cfg(rudzio_test)'] }}\n\
             \n\
             [profile.dev]\n\
             panic = \"abort\"\n\
             \n\
             [profile.release]\n\
             panic = \"abort\"\n"
        ),
    )
}

/// Build a lib-member manifest. `extra` is inserted between the `[lib]`
/// table and the `[dev-dependencies]` table (the `[[test]]` entries).
fn lib_member_manifest(name: &str, extra: &str) -> String {
    format!(
        "[package]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"{name}\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         {extra}\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n"
    )
}

/// Materialise a lib member: manifest + a minimal `src/lib.rs`.
fn write_lib_member(dir: &Path, name: &str, extra: &str) -> io::Result<()> {
    fs::create_dir_all(dir.join("src"))?;
    fs::create_dir_all(dir.join("tests"))?;
    fs::write(dir.join("Cargo.toml"), lib_member_manifest(name, extra))?;
    fs::write(
        dir.join("src").join("lib.rs"),
        format!("//! Minimal `{name}` lib fixture.\n"),
    )
}

/// Build a bin-only member manifest (a `[[bin]]`, no `[lib]`). `extra` is
/// inserted after the `[[bin]]` table (the `[[test]]` entries).
fn bin_only_member_manifest(name: &str, extra: &str) -> String {
    format!(
        "[package]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         autotests = false\n\
         \n\
         [[bin]]\n\
         name = \"{name}\"\n\
         path = \"src/main.rs\"\n\
         \n\
         {extra}\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n"
    )
}

/// Materialise a bin-only member: manifest + a minimal `src/main.rs`.
fn write_bin_only_member(dir: &Path, name: &str, extra: &str) -> io::Result<()> {
    fs::create_dir_all(dir.join("src"))?;
    fs::create_dir_all(dir.join("tests"))?;
    fs::write(
        dir.join("Cargo.toml"),
        bin_only_member_manifest(name, extra),
    )?;
    fs::write(dir.join("src").join("main.rs"), "fn main() {}\n")
}

/// The `tests/main.rs` shim body: the declared `mod` decls followed by the
/// `#[rudzio::main]` entry point.
fn shim(mod_decls: &str) -> String {
    format!("{mod_decls}\n#[rudzio::main]\nfn main() {{}}\n")
}

/// A flat test file carrying a single `#[rudzio::test]` fn named
/// `test_name` under the `futures::ThreadPool` runtime. `imports` is the
/// (already indented) body preamble of `mod tests`; `body` is the (already
/// indented) test body.
fn futures_test_file(test_name: &str, imports: &str, body: &str) -> String {
    format!(
        "//! `{test_name}` fixture test.\n\
         \n\
         #[rudzio::suite([\n\
         \x20\x20\x20\x20(runtime = rudzio::runtime::futures::ThreadPool::new,\n\
         \x20\x20\x20\x20 suite = rudzio::common::context::Suite,\n\
         \x20\x20\x20\x20 test = rudzio::common::context::Test),\n\
         ])]\n\
         mod tests {{\n\
         {imports}\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20async fn {test_name}(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {{\n\
         {body}\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}}\n\
         }}\n"
    )
}

/// Warm the per-test cargo home's git cache for the fixture's `file://` git
/// dependency, then the fixture is run offline by the caller.
fn prepopulate_git_dep(cargo_home: &Path, fixture_root: &Path) -> anyhow::Result<()> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo_bin)
        .arg("fetch")
        .arg("--manifest-path")
        .arg(fixture_root.join("Cargo.toml"))
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_NET_OFFLINE", "false")
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(io::Error::other)?;
    anyhow::ensure!(
        output.status.success(),
        "pre-fetching the git dependency for {} failed: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        fixture_root.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// Drive `cargo rudzio test` against `fixture_root` with the
/// freshness-checked cargo-rudzio binary and an offline (pre-warmed) cargo
/// home. The inner build writes to per-test target/build dirs.
fn run_cargo_rudzio(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    fixture_root: &Path,
) -> anyhow::Result<Output> {
    let binary = assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;
    let output = Command::new(binary)
        .arg("test")
        .current_dir(fixture_root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(io::Error::other)?;
    Ok(output)
}

/// Classify a `cargo rudzio test` run against the surface-visibility
/// invariant. `marker_name` is the test fn name; `marker_path` is the
/// member-relative path of the file holding it.
fn classify(output: &Output, marker_name: &str, marker_path: &str) -> InvariantOutcome {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let marker_ran = stdout.contains(marker_name);
    if output.status.success() && marker_ran {
        return InvariantOutcome::Visible;
    }
    if !output.status.success() && (stderr.contains(marker_path) || stderr.contains(marker_name)) {
        return InvariantOutcome::LoudlyRefused;
    }
    InvariantOutcome::SilentlyDisappeared
}

/// Error for the `SilentlyDisappeared` verdict.
fn silent_disappearance_failure(
    marker_path: &str,
    marker_name: &str,
    output: &Output,
) -> anyhow::Error {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::anyhow!(
        "surface-visibility invariant violated for `{marker_path}`:\n\
         `cargo rudzio test` exited successfully but `{marker_name}` is absent from the \
         captured output AND no stderr message names the file. The inner suite silently \
         disappeared into a compile unit the aggregator does not link. The inner test must \
         run, or the tool must fail loudly; a silent drop is never correct.\n\
         --- captured stdout ---\n{stdout}\n\
         --- captured stderr ---\n{stderr}",
    )
}

/// Build a task-local cargo build dir beside the cargo target dir.
fn cargo_build_dir_for(cargo_target_dir: &Path) -> anyhow::Result<PathBuf> {
    Ok(cargo_target_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
        .join("cargo-build-dir"))
}

/// Materialise the workspace + prepopulate the git dep + run the runner,
/// returning the captured output.
fn drive(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    fixture_root: &Path,
) -> anyhow::Result<Output> {
    let cargo_build_dir = cargo_build_dir_for(cargo_target_dir)?;
    prepopulate_git_dep(cargo_home, fixture_root)?;
    run_cargo_rudzio(cargo_home, cargo_target_dir, &cargo_build_dir, fixture_root)
}

/// Case 1: a lib member whose shim declares a single directory helper.
fn write_case1(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_lib_member(&consumer, "consumer", MAIN_SHIM_TEST_ENTRY)?;
    fs::create_dir_all(consumer.join("tests").join("harness"))?;
    fs::write(
        consumer.join("tests").join("main.rs"),
        shim("mod smoke;\nmod harness;"),
    )?;
    fs::write(
        consumer.join("tests").join("harness").join("mod.rs"),
        "//! Shared harness.\n\npub fn answer() -> u32 { 42 }\n",
    )?;
    fs::write(
        consumer.join("tests").join("smoke.rs"),
        futures_test_file(
            "smoke_uses_harness",
            "    use crate::harness::answer;\n",
            "        anyhow::ensure!(answer() == 42);\n",
        ),
    )?;
    Ok(root.to_path_buf())
}

/// Case 2: a lib member whose shim declares two directory helpers.
fn write_case2(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_lib_member(&consumer, "consumer", MAIN_SHIM_TEST_ENTRY)?;
    fs::create_dir_all(consumer.join("tests").join("harness"))?;
    fs::create_dir_all(consumer.join("tests").join("fixtures"))?;
    fs::write(
        consumer.join("tests").join("main.rs"),
        shim("mod smoke;\nmod harness;\nmod fixtures;"),
    )?;
    fs::write(
        consumer.join("tests").join("harness").join("mod.rs"),
        "//! Shared harness.\n\npub fn answer() -> u32 { 42 }\n",
    )?;
    fs::write(
        consumer.join("tests").join("fixtures").join("mod.rs"),
        "//! Shared fixtures.\n\npub fn expected() -> u32 { 7 }\n",
    )?;
    fs::write(
        consumer.join("tests").join("smoke.rs"),
        futures_test_file(
            "smoke_uses_two_helpers",
            "    use crate::fixtures::expected;\n    use crate::harness::answer;\n",
            "        anyhow::ensure!(answer() == expected() * 6);\n",
        ),
    )?;
    Ok(root.to_path_buf())
}

/// Case 3: a helper carrying a non-Rust binary fixture read via `include_bytes`.
fn write_case3(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_lib_member(&consumer, "consumer", MAIN_SHIM_TEST_ENTRY)?;
    fs::create_dir_all(consumer.join("tests").join("harness").join("data"))?;
    fs::write(
        consumer.join("tests").join("main.rs"),
        shim("mod smoke;\nmod harness;"),
    )?;
    fs::write(
        consumer.join("tests").join("harness").join("mod.rs"),
        "//! Shared harness with a non-Rust binary fixture.\n\n\
         pub fn sample_bytes() -> &'static [u8] {\n    include_bytes!(\"data/sample.bin\")\n}\n",
    )?;
    fs::write(
        consumer
            .join("tests")
            .join("harness")
            .join("data")
            .join("sample.bin"),
        [0x00_u8, 0x01, 0x02, 0x03],
    )?;
    fs::write(
        consumer.join("tests").join("smoke.rs"),
        futures_test_file(
            "smoke_reads_binary_fixture",
            "    use crate::harness::sample_bytes;\n",
            "        anyhow::ensure!(sample_bytes() == [0x00, 0x01, 0x02, 0x03]);\n",
        ),
    )?;
    Ok(root.to_path_buf())
}

/// Case 4: a flat test root (no shim) discovering a sibling helper.
fn write_case4(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_lib_member(
        &consumer,
        "consumer",
        "[[test]]\nname = \"other\"\npath = \"tests/other.rs\"\nharness = false\n",
    )?;
    fs::write(
        consumer.join("tests").join("helper.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )?;
    fs::write(
        consumer.join("tests").join("other.rs"),
        "//! Flat test root (no `tests/main.rs` shim).\n\n\
         mod helper;\n\n\
         #[rudzio::suite([\n\
         \x20\x20\x20\x20(runtime = rudzio::runtime::futures::ThreadPool::new,\n\
         \x20\x20\x20\x20 suite = rudzio::common::context::Suite,\n\
         \x20\x20\x20\x20 test = rudzio::common::context::Test),\n\
         ])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use crate::helper::answer;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20async fn flat_root_uses_helper(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;
    Ok(root.to_path_buf())
}

/// Case 5: two sibling test files aggregated under one shim.
fn write_case5(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_lib_member(&consumer, "consumer", MAIN_SHIM_TEST_ENTRY)?;
    fs::write(
        consumer.join("tests").join("main.rs"),
        shim("mod a;\nmod b;"),
    )?;
    fs::write(
        consumer.join("tests").join("a.rs"),
        futures_test_file("a_test", "", "        anyhow::ensure!(1 + 1 == 2);\n"),
    )?;
    fs::write(
        consumer.join("tests").join("b.rs"),
        futures_test_file("b_test", "", "        anyhow::ensure!(2 + 2 == 4);\n"),
    )?;
    Ok(root.to_path_buf())
}

/// Case 6: a helper with a nested submodule carried through hoisting.
fn write_case6(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_lib_member(&consumer, "consumer", MAIN_SHIM_TEST_ENTRY)?;
    fs::create_dir_all(consumer.join("tests").join("harness").join("fixtures"))?;
    fs::write(
        consumer.join("tests").join("main.rs"),
        shim("mod smoke;\nmod harness;"),
    )?;
    fs::write(
        consumer.join("tests").join("harness").join("mod.rs"),
        "//! Shared harness with a nested submodule.\n\n\
         pub mod fixtures;\n\n\
         pub fn answer() -> u32 { 42 }\n",
    )?;
    fs::write(
        consumer
            .join("tests")
            .join("harness")
            .join("fixtures")
            .join("mod.rs"),
        "//! Nested fixture helpers.\n\npub const EXPECTED: u32 = 42;\n",
    )?;
    fs::write(
        consumer.join("tests").join("smoke.rs"),
        futures_test_file(
            "smoke_uses_nested_helper",
            "    use crate::harness::answer;\n    use crate::harness::fixtures::EXPECTED;\n",
            "        anyhow::ensure!(answer() == EXPECTED);\n",
        ),
    )?;
    Ok(root.to_path_buf())
}

/// Case 7: a bin-only member whose shim declares a test module and a helper.
fn write_case7(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_bin_only_member(&consumer, "consumer", MAIN_SHIM_TEST_ENTRY)?;
    fs::create_dir_all(consumer.join("tests").join("harness"))?;
    fs::write(
        consumer.join("tests").join("main.rs"),
        shim("mod cli;\nmod harness;"),
    )?;
    fs::write(
        consumer.join("tests").join("harness").join("mod.rs"),
        "//! Shared harness for the bin.\n\npub fn app_name() -> &'static str { \"consumer\" }\n",
    )?;
    fs::write(
        consumer.join("tests").join("cli.rs"),
        futures_test_file(
            "cli_uses_harness_and_bin",
            "    use crate::harness::app_name;\n",
            "        anyhow::ensure!(app_name() == \"consumer\");\n\
             \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(!env!(\"CARGO_BIN_EXE_consumer\").is_empty());\n",
        ),
    )?;
    Ok(root.to_path_buf())
}

/// Case 8: a lib+bin member with a non-default bin resolved via `CARGO_BIN_EXE`.
fn write_case8(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let mixed = root.join("crates").join("mixed");
    fs::create_dir_all(mixed.join("src").join("bin"))?;
    fs::create_dir_all(mixed.join("tests"))?;
    fs::write(
        mixed.join("Cargo.toml"),
        "[package]\n\
         name = \"mixed\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"mixed\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"mixed\"\n\
         path = \"src/main.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"extra\"\n\
         path = \"src/bin/extra.rs\"\n\
         \n\
         [[test]]\n\
         name = \"main\"\n\
         path = \"tests/main.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n",
    )?;
    fs::write(
        mixed.join("src").join("lib.rs"),
        "//! Mixed lib with a src test suite.\n\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20#[rudzio::suite([\n\
         \x20\x20\x20\x20\x20\x20\x20\x20(runtime = rudzio::runtime::futures::ThreadPool::new,\n\
         \x20\x20\x20\x20\x20\x20\x20\x20 suite = rudzio::common::context::Suite,\n\
         \x20\x20\x20\x20\x20\x20\x20\x20 test = rudzio::common::context::Test),\n\
         \x20\x20\x20\x20])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20async fn lib_suite_test(_t: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(2 + 2 == 4);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    fs::write(mixed.join("src").join("main.rs"), "fn main() {}\n")?;
    fs::write(
        mixed.join("src").join("bin").join("extra.rs"),
        "fn main() {}\n",
    )?;
    fs::write(
        mixed.join("tests").join("main.rs"),
        shim("mod integration;"),
    )?;
    fs::write(
        mixed.join("tests").join("integration.rs"),
        futures_test_file(
            "integration_runs_extra_bin",
            "",
            "        anyhow::ensure!(!env!(\"CARGO_BIN_EXE_extra\").is_empty());\n",
        ),
    )?;
    Ok(root.to_path_buf())
}

/// Case 9: a shim-declared suite file whose nested inner module holds the suite.
fn write_case9(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    write_lib_member(&consumer, "consumer", MAIN_SHIM_TEST_ENTRY)?;
    fs::create_dir_all(consumer.join("tests").join("suite_a"))?;
    fs::write(consumer.join("tests").join("main.rs"), shim("mod suite_a;"))?;
    fs::write(consumer.join("tests").join("suite_a.rs"), "mod inner;\n")?;
    fs::write(
        consumer.join("tests").join("suite_a").join("inner.rs"),
        futures_test_file("inner_test", "", "        anyhow::ensure!(6 * 7 == 42);\n"),
    )?;
    Ok(root.to_path_buf())
}

/// Case 10: autotests default discovery with a tests/common/mod.rs helper.
fn write_case10(root: &Path, rev: &str) -> io::Result<PathBuf> {
    write_workspace_manifest(root, rev)?;
    let consumer = root.join("crates").join("consumer");
    fs::create_dir_all(consumer.join("src"))?;
    fs::create_dir_all(consumer.join("tests").join("common"))?;
    fs::write(
        consumer.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n",
    )?;
    fs::write(
        consumer.join("src").join("lib.rs"),
        "//! Minimal consumer lib.\n",
    )?;
    fs::write(
        consumer.join("tests").join("common").join("mod.rs"),
        "//! Shared helper for the autotests member.\n\npub fn answer() -> u32 { 42 }\n",
    )?;
    // Unlike the shim cases, `autotests = true` has no `tests/main.rs` to
    // declare `mod common;`, so the flat test file declares the helper
    // module itself (the canonical `tests/common/mod.rs` pattern).
    fs::write(
        consumer.join("tests").join("a.rs"),
        "//! `autotests_a_uses_common` fixture test.\n\n\
         mod common;\n\n\
         #[rudzio::suite([\n\
         \x20\x20\x20\x20(runtime = rudzio::runtime::futures::ThreadPool::new,\n\
         \x20\x20\x20\x20 suite = rudzio::common::context::Suite,\n\
         \x20\x20\x20\x20 test = rudzio::common::context::Test),\n\
         ])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use crate::common::answer;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20async fn autotests_a_uses_common(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(answer() == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    Ok(root.to_path_buf())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        InvariantOutcome, IsolatedCargoTest, RunSummary, assert_exact_test_names, assert_summary,
        classify, combined_output, drive, rudzio_git_rev, silent_disappearance_failure,
        write_case1, write_case2, write_case3, write_case4, write_case5, write_case6, write_case7,
        write_case8, write_case9, write_case10,
    };

    /// Case 1: shim-declared single directory helper hoists.
    #[rudzio::test]
    async fn case1_shim_single_directory_helper(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case1(&ctx.suite_tempdir().join("case1"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["consumer::smoke::smoke_uses_harness"])
    }

    /// Case 2: shim-declared two directory helpers both hoist.
    #[rudzio::test]
    async fn case2_shim_multiple_directory_helpers(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case2(&ctx.suite_tempdir().join("case2"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["consumer::smoke::smoke_uses_two_helpers"])
    }

    /// Case 3: non-Rust binary fixture inside a helper is copied verbatim.
    #[rudzio::test]
    async fn case3_helper_with_non_rust_data_fixture(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case3(&ctx.suite_tempdir().join("case3"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["consumer::smoke::smoke_reads_binary_fixture"])
    }

    /// Case 4: flat `tests/other.rs` root (no shim) discovers its sibling helper.
    #[rudzio::test]
    async fn case4_flat_test_root_without_shim(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case4(&ctx.suite_tempdir().join("case4"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["consumer::other::flat_root_uses_helper"])
    }

    /// Case 5: two sibling test files under one shim aggregate exactly.
    #[rudzio::test]
    async fn case5_two_siblings_under_shim(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case5(&ctx.suite_tempdir().join("case5"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(2, 0, 0, 0, 0, 0, 0, 2))?;
        assert_exact_test_names(&text, &["consumer::a::a_test", "consumer::b::b_test"])
    }

    /// Case 6: helper with a nested submodule carries its submodule through hoisting.
    #[rudzio::test]
    async fn case6_helper_with_nested_submodule(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case6(&ctx.suite_tempdir().join("case6"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["consumer::smoke::smoke_uses_nested_helper"])
    }

    /// Case 7: bin-only shim declares a test module AND a helper.
    #[rudzio::test]
    async fn case7_bin_only_shim_test_module_and_helper(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case7(&ctx.suite_tempdir().join("case7"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["consumer::cli::cli_uses_harness_and_bin"])
    }

    /// Case 8: lib+bin+`src/bin/extra.rs`; integration test resolves
    /// `CARGO_BIN_EXE_extra`; the src lib suite also runs.
    #[rudzio::test]
    async fn case8_lib_bin_extra_bin_cargo_bin_exe(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case8(&ctx.suite_tempdir().join("case8"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(2, 0, 0, 0, 0, 0, 0, 2))?;
        assert_exact_test_names(
            &text,
            &[
                "lib_suite_test",
                "mixed::integration::integration_runs_extra_bin",
            ],
        )
    }

    /// Case 9: `mod suite_a;` whose nested `suite_a/inner.rs` holds the suite —
    /// must be visible or loudly refused, never silently dropped.
    #[rudzio::test]
    async fn case9_shim_suite_a_nested_inner_suite(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case9(&ctx.suite_tempdir().join("case9"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        match classify(&output, "inner_test", "suite_a/inner.rs") {
            InvariantOutcome::Visible | InvariantOutcome::LoudlyRefused => Ok(()),
            InvariantOutcome::SilentlyDisappeared => Err(silent_disappearance_failure(
                "suite_a/inner.rs",
                "inner_test",
                &output,
            )),
        }
    }

    /// Case 10: `autotests = true` default discovery with a
    /// `tests/common/mod.rs` helper.
    #[rudzio::test]
    async fn case10_autotests_default_discovery_common_helper(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let rev = rudzio_git_rev()?;
        let root = write_case10(&ctx.suite_tempdir().join("case10"), &rev)?;
        let output = drive(ctx.cargo_home(), ctx.cargo_target_dir(), &root)?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );
        assert_summary(&text, RunSummary::plain(1, 0, 0, 0, 0, 0, 0, 1))?;
        assert_exact_test_names(&text, &["consumer::a::autotests_a_uses_common"])
    }
}
