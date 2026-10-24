//! Ground-truth e2e reproducer for the operator's REAL Bug #8 crate shape.
//!
//! The operator's `modera` crate is a BIN-ONLY workspace member (only
//! `src/main.rs`, no `[lib]`) whose integration tests live in
//! `tests/cli.rs` as a SUBMODULE of a separate runner shim
//! `tests/main.rs` (`mod cli; #[rudzio::main] fn main() {}`). The manifest
//! declares `autotests = false` plus a single
//! `[[test]] name = "main" path = "tests/main.rs" harness = false` entry.
//!
//! This differs from every prior bin-only spec in one load-bearing way:
//! the test file (`tests/cli.rs`) carries **no `#[rudzio::main]` of its
//! own** — it is a plain submodule the runner shim `mod cli;`-declares.
//! The prior `edge_bin_only_e2e_spec` and `ground_truth_bug_08` shapes both
//! put `#[rudzio::main]` directly in the discovered test file, so their
//! proxies self-contained a runner entry the operator's real crate never
//! has.
//!
//! # Symptom (pre-fix)
//!
//! ```text
//! thread 'main' panicked at .../linkme-.../distributed_slice.rs:246:
//! unsafe precondition(s) violated: slice::from_raw_parts requires the
//! pointer to be aligned and non-null ...
//! ```
//! (SIGABRT, exit 134) — the aggregator's `TEST_TOKENS` slice is empty.
//!
//! # Fresh build (no stale binary)
//!
//! Drives `cargo run -p cargo-rudzio -- test` (never a `PATH` binary), with
//! `RUSTC_WRAPPER=` + `SCCACHE_DISABLE=1` + task-local `CARGO_TARGET_DIR` /
//! `CARGO_BUILD_BUILD_DIR`.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// The runner shim: declares the `cli` submodule and the `#[rudzio::main]`
/// entry point. The tests themselves live in `tests/cli.rs` and carry no
/// `#[rudzio::main]`.
const MAIN_SHIM_SRC: &str = "//! Single runner entry for the whole modera test binary.\n\
mod cli;\n\
\n\
#[rudzio::main]\n\
fn main() {}\n";

/// `tests/cli.rs` carrying exactly three `#[rudzio::test]` fns, no
/// `#[rudzio::main]`.
const CLI_TEST_SRC: &str = "//! modera CLI integration tests.\n\
use rudzio::common::context::{Suite, Test};\n\
use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
\n\
#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
mod tests {\n\
    #[rudzio::test]\n\
    fn cli_parse_help() -> anyhow::Result<()> {\n\
        anyhow::ensure!(2 + 2 == 4);\n\
        Ok(())\n\
    }\n\
\n\
    #[rudzio::test]\n\
    fn cli_parse_version() -> anyhow::Result<()> {\n\
        anyhow::ensure!(3 + 3 == 6);\n\
        Ok(())\n\
    }\n\
\n\
    #[rudzio::test]\n\
    fn cli_parse_run() -> anyhow::Result<()> {\n\
        anyhow::ensure!(4 + 4 == 8);\n\
        Ok(())\n\
    }\n\
}\n";

/// Repo root = parent of `cargo-rudzio/` (the rudzio workspace).
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
}

/// Cargo home that already holds cargo-rudzio's full dependency graph
/// (`toml_edit`, `cargo_metadata`, clap, …). `cargo run -p cargo-rudzio`
/// must build against a home with the complete graph; the per-test
/// hardlinked home only pre-warms anyhow/tokio/rudzio.
fn cargo_home_with_full_graph() -> PathBuf {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// Drive `cargo run -p cargo-rudzio -- test <args>` from the repo root with
/// a FRESH build.
fn run_cargo_rudzio_fresh(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    args: &[&str],
) -> io::Result<Output> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Command::new(cargo_bin)
        .arg("run")
        .arg("-p")
        .arg("cargo-rudzio")
        .arg("--")
        .arg("test")
        .args(args)
        .current_dir(repo_root())
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .env("CARGO_NET_OFFLINE", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Write the workspace root manifest listing `members`, with a workspace
/// `rudzio` dev-dep pointing at the repo root.
fn write_workspace_manifest(root: &Path, members: &[&str]) -> io::Result<()> {
    let rudzio_root = repo_root();
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

/// Write `modera`'s bin-only manifest: `[[bin]]` (no `[lib]`),
/// `autotests = false`, and a single
/// `[[test]] name = "main" path = "tests/main.rs" harness = false` runner
/// shim. `tests/cli.rs` is a submodule of that shim, not its own target.
fn write_modera_manifest(modera_dir: &Path) -> io::Result<()> {
    fs::write(
        modera_dir.join("Cargo.toml"),
        "[package]\n\
         name = \"modera\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [[bin]]\n\
         name = \"modera\"\n\
         path = \"src/main.rs\"\n\
         \n\
         [[test]]\n\
         name = \"main\"\n\
         path = \"tests/main.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )
}

/// Materialise the operator's bin-only workspace-member `modera` crate at
/// `workspace/modera`.
fn write_modera_crate(workspace: &Path, modera_dir: &Path) -> io::Result<()> {
    let src = modera_dir.join("src");
    let tests = modera_dir.join("tests");
    fs::create_dir_all(&src)?;
    fs::create_dir_all(&tests)?;
    write_workspace_manifest(workspace, &["modera"])?;
    write_modera_manifest(modera_dir)?;
    fs::write(src.join("main.rs"), "fn main() {}\n")?;
    fs::write(tests.join("main.rs"), MAIN_SHIM_SRC)?;
    fs::write(tests.join("cli.rs"), CLI_TEST_SRC)?;
    Ok(())
}

/// Write `modera`'s STANDALONE bin-only manifest: `[[bin]]` (no `[lib]`),
/// `autotests = false`, a `tests/main.rs` runner shim, and **no
/// `[workspace]` table** — so the crate is its own workspace root. This is
/// the shape the operator's repro hits via
/// `cargo rudzio test --manifest-path <crate>/Cargo.toml -p modera`.
fn write_standalone_modera_manifest(modera_dir: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        modera_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"modera\"\n\
             version = \"0.1.0\"\n\
             edition = \"2024\"\n\
             publish = false\n\
             autotests = false\n\
             \n\
             [[bin]]\n\
             name = \"modera\"\n\
             path = \"src/main.rs\"\n\
             \n\
             [[test]]\n\
             name = \"main\"\n\
             path = \"tests/main.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\n\
             anyhow = \"=1.0.104\"\n",
            rudzio = rudzio_root.display(),
        ),
    )
}

/// Materialise the operator's STANDALONE bin-only `modera` crate at
/// `crate_dir` (its own workspace root, no enclosing `[workspace]`).
fn write_standalone_modera_crate(crate_dir: &Path, rudzio_root: &Path) -> io::Result<()> {
    let src = crate_dir.join("src");
    let tests = crate_dir.join("tests");
    fs::create_dir_all(&src)?;
    fs::create_dir_all(&tests)?;
    write_standalone_modera_manifest(crate_dir, rudzio_root)?;
    fs::write(src.join("main.rs"), "fn main() {}\n")?;
    fs::write(tests.join("main.rs"), MAIN_SHIM_SRC)?;
    fs::write(tests.join("cli.rs"), CLI_TEST_SRC)?;
    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        IsolatedCargoTest, cargo_home_with_full_graph, repo_root, run_cargo_rudzio_fresh,
        write_modera_crate, write_standalone_modera_crate,
    };

    /// **Bug #8 (operator's real shape)** — a bin-only workspace member whose
    /// tests are a `tests/cli.rs` submodule of a separate `tests/main.rs`
    /// runner shim must aggregate all its `#[rudzio::test]` fns.
    #[rudzio::test]
    async fn modera_bin_only_cli_submodule_aggregates_all_tests(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("modera-workspace");
        let modera_dir = workspace.join("modera");
        write_modera_crate(&workspace, &modera_dir)?;

        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        let workspace_manifest = workspace.join("Cargo.toml").to_string_lossy().into_owned();
        let output = run_cargo_rudzio_fresh(
            &cargo_home_with_full_graph(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &["--manifest-path", &workspace_manifest, "-p", "modera"],
        )?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::ensure!(
            output.status.success(),
            "bin-only member `modera` must run its `tests/cli.rs` tests (exit 0), but the runner \
             exited {:?}. Bug #8: TEST_TOKENS is empty and linkme aborts.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code(),
        );
        for expected in ["cli_parse_help", "cli_parse_version", "cli_parse_run"] {
            anyhow::ensure!(
                stdout.contains(expected),
                "runner stdout must name the aggregated test `{expected}`.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            );
        }
        Ok(())
    }

    /// **Bug #8 (operator's real shape, standalone)** — a STANDALONE
    /// bin-only crate (`manifest_dir == workspace_root`, no `[workspace]`)
    /// whose tests are a `tests/cli.rs` submodule of a `tests/main.rs`
    /// runner shim must aggregate all its `#[rudzio::test]` fns.
    #[rudzio::test]
    async fn modera_standalone_cli_submodule_aggregates_all_tests(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let crate_dir = ctx.suite_tempdir().join("standalone-modera");
        write_standalone_modera_crate(&crate_dir, repo_root())?;

        let cargo_build_dir = ctx
            .cargo_target_dir()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir");

        let manifest = crate_dir.join("Cargo.toml").to_string_lossy().into_owned();
        let output = run_cargo_rudzio_fresh(
            &cargo_home_with_full_graph(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &["--manifest-path", &manifest, "-p", "modera"],
        )?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::ensure!(
            output.status.success(),
            "standalone bin-only crate `modera` must run its `tests/cli.rs` tests (exit 0), but \
             the runner exited {:?}. Bug #8: TEST_TOKENS is empty and linkme aborts.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code(),
        );
        for expected in ["cli_parse_help", "cli_parse_version", "cli_parse_run"] {
            anyhow::ensure!(
                stdout.contains(expected),
                "runner stdout must name the aggregated test `{expected}`.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            );
        }
        Ok(())
    }
}
