//! End-to-end spec for per-`[[bin]]` registry isolation.
//!
//! # Bug
//!
//! A standalone `#[rudzio::main]` fixture binary (a `[[bin]]` whose own
//! source self-aggregates via `#[rudzio::main]`) must run ONLY its own
//! `#[rudzio::test]` / `#[rudzio::suite]` surface. When that binary is a
//! member of a workspace whose OTHER members carry bridged src tests, the
//! bridge collapsed every member's test registry into one linkme
//! `TEST_TOKENS` slice, and each `[[bin]]` binary linked that same full
//! registry — so `CARGO_BIN_EXE_<bin>` ran the whole workspace's tests
//! instead of its own.
//!
//! # Shape
//!
//! * `deps` — a lib member whose `src/lib.rs` carries two `#[rudzio::test]`
//!   items (bridged, so they register into the aggregator).
//! * `bins` — a bin-only member with one standalone `[[bin]]` (`alpha`,
//!   one `#[rudzio::test]`) that `use deps::answer;`, plus an integration
//!   `[[test]]` that spawns `CARGO_BIN_EXE_alpha` and asserts it reports
//!   exactly `1 passed` and never the `deps` test names.
//!
//! Pre-fix the `alpha` binary links `deps`'s bridge (via the sibling-dep
//! redirect) and therefore runs `deps`'s tests too. Post-fix the standalone
//! bin links the REAL `deps` lib (no test surface) and reports only its own.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{
    IsolatedCargoSuite, IsolatedCargoTest, RunSummary, assert_exact_test_names, assert_summary,
    combined_output,
};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/`.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Workspace root manifest listing `members`, with `rudzio` + `anyhow`
/// workspace deps pointed at the code under test.
fn write_workspace_manifest(root: &Path, members: &[&str]) -> io::Result<()> {
    let rudzio_root = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    let member_list = members
        .iter()
        .map(|member| format!("\"{member}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [{member_list}]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )
}

/// `deps` member manifest: a `[lib]` (so src tests bridge) with a
/// `rudzio` dev-dep.
fn write_deps_manifest(deps_dir: &Path) -> io::Result<()> {
    fs::write(
        deps_dir.join("Cargo.toml"),
        "[package]\n\
         name = \"deps\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [dependencies]\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )
}

/// `deps` lib source: one public fn plus two `#[rudzio::test]` src items
/// gated behind `cfg(any(test, rudzio_test))`.
fn write_deps_lib(deps_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(deps_dir.join("src"))?;
    fs::write(
        deps_dir.join("src").join("lib.rs"),
        "//! deps lib\n\
         \n\
         /// The answer.\n\
         pub fn answer() -> u32 {\n\
         \x20\x20\x20\x2042\n\
         }\n\
         \n\
         #[cfg(any(test, rudzio_test))]\n\
         #[allow(clippy::tests_outside_test_module)]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn deps_test_a() -> anyhow::Result<()> { Ok(()) }\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn deps_test_b() -> anyhow::Result<()> { Ok(()) }\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )
}

/// `bins` member manifest: a standalone `[[bin]]` (`alpha`) plus an
/// integration `[[test]]` (`autotests = false`, harness = false).
fn write_bins_manifest(bins_dir: &Path) -> io::Result<()> {
    fs::write(
        bins_dir.join("Cargo.toml"),
        "[package]\n\
         name = \"bins\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [[bin]]\n\
         name = \"alpha\"\n\
         path = \"src/bin/alpha.rs\"\n\
         test = false\n\
         \n\
         [[test]]\n\
         name = \"main\"\n\
         path = \"tests/main.rs\"\n\
         harness = false\n\
         \n\
         [dependencies]\n\
         deps = { path = \"../deps\" }\n\
         anyhow = { workspace = true }\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n",
    )
}

/// `alpha` standalone fixture bin: one test + a `deps` import so the
/// `deps` dep is actually linked.
fn write_alpha_bin(bins_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(bins_dir.join("src").join("bin"))?;
    fs::write(
        bins_dir.join("src").join("bin").join("alpha.rs"),
        "use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread;\n\
         \n\
         #[rudzio::suite([(runtime = Multithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use super::Test;\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn alpha_one(_ctx: &Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(deps::answer() == 42);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )
}

/// `bins` integration test shim + sibling module spawning `alpha`.
fn write_bins_tests(bins_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(bins_dir.join("tests"))?;
    fs::write(
        bins_dir.join("tests").join("main.rs"),
        "mod integration;\n\n#[rudzio::main]\nfn main() {}\n",
    )?;
    fs::write(
        bins_dir.join("tests").join("integration.rs"),
        "use std::process::Command;\n\
         use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread;\n\
         \n\
         #[rudzio::suite([(runtime = Multithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use super::{Command, Test};\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn alpha_is_isolated(_ctx: &Test) -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let out = Command::new(env!(\"CARGO_BIN_EXE_alpha\")).output()?;\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let log = String::from_utf8_lossy(&out.stdout);\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(log.contains(\"1 passed\"), \"alpha must report 1 passed:\\n{log}\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(log.contains(\"alpha_one\"), \"alpha must run alpha_one:\\n{log}\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(!log.contains(\"deps_test_a\"), \"alpha must not run deps_test_a:\\n{log}\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20anyhow::ensure!(!log.contains(\"deps_test_b\"), \"alpha must not run deps_test_b:\\n{log}\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )
}

/// Spawn `cargo rudzio test` against `root` with the isolated cargo env.
fn run_cargo_rudzio(root: &Path, cargo_home: &Path, cargo_target_dir: &Path) -> io::Result<Output> {
    Command::new(
        cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .map_err(io::Error::other)?,
    )
    .arg("test")
    .current_dir(root)
    .env(
        "CARGO",
        env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
    )
    .env("CARGO_HOME", cargo_home)
    .env("CARGO_TARGET_DIR", cargo_target_dir)
    .env("CARGO_NET_OFFLINE", "true")
    .env("NO_COLOR", "1")
    .output()
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        IsolatedCargoTest, Path, RunSummary, TempDir, assert_exact_test_names, assert_summary,
        combined_output, fs, run_cargo_rudzio, write_alpha_bin, write_bins_manifest,
        write_bins_tests, write_deps_lib, write_deps_manifest, write_workspace_manifest,
    };

    #[rudzio::test]
    fn standalone_bin_runs_only_its_own_tests(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_workspace_manifest(root, &["deps", "bins"])?;

        let deps = root.join("deps");
        fs::create_dir_all(&deps)?;
        write_deps_manifest(&deps)?;
        write_deps_lib(&deps)?;

        let bins = root.join("bins");
        fs::create_dir_all(&bins)?;
        write_bins_manifest(&bins)?;
        write_alpha_bin(&bins)?;
        write_bins_tests(&bins)?;

        let output = run_cargo_rudzio(root, ctx.cargo_home(), ctx.cargo_target_dir())?;
        let text = combined_output(&output);
        anyhow::ensure!(
            output.status.success(),
            "cargo rudzio test must exit 0; exit = {:?}\n--- output ---\n{text}",
            output.status.code(),
        );

        // The aggregator runs deps' two src tests plus the one integration
        // test that spawns alpha (and asserts alpha's own isolation).
        assert_summary(&text, RunSummary::plain(3, 0, 0, 0, 0, 0, 0, 3))?;
        assert_exact_test_names(
            &text,
            &[
                "deps_test_a",
                "deps_test_b",
                "bins::integration::alpha_is_isolated",
            ],
        )?;
        Ok(())
    }
}
