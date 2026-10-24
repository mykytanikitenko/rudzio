//! Faithful e2e reproducer for the operator's REAL Bug #8 `modera` crate.
//!
//! This is the verbatim shape the operator verified, materialised with zero
//! approximation. It differs from [`super::migration_bug_08_modera_cli_e2e_spec`]
//! in every load-bearing detail that earlier "passing" fixtures dropped:
//!
//! * the `rudzio` dependency is a **git** dependency (`git = "file://…"` +
//!   `rev = "…"`) declared once in `[workspace.dependencies]` and inherited via
//!   `rudzio.workspace = true` — not a `path` dependency;
//! * the runtime features are `runtime-futures` + `runtime-compio` + `common`
//!   (not `runtime-tokio-multi-thread`), and the suite drives
//!   `rudzio::runtime::futures::ThreadPool`;
//! * the workspace pins `resolver = "2"` (not `"3"`);
//! * `edition` is inherited from `[workspace.package] edition = "2024"`;
//! * `[workspace.lints.rust]` denies `warnings` and `unsafe_code` and
//!   declares `check-cfg = ['cfg(rudzio_test)']`;
//! * `[profile.dev]`/`[profile.release]` set `panic = "abort"`;
//! * a `.cargo/config.toml` sets `build-dir = "tmp/build-dir"`;
//! * the bin pulls `output` (workspace path dep) and `pico-args` and handles
//!   `-V`/`--version`/`-h`/`--help` by printing and exiting 0.
//!
//! The `modera` member is bin-only (`src/main.rs`, no `[lib]`), declares
//! `autotests = false` plus a single `[[test]] name = "main" path =
//! "tests/main.rs" harness = false` runner shim (`mod cli; #[rudzio::main] fn
//! main() {}`), and `tests/cli.rs` is a FLAT file (no `tests/cli/mod.rs`)
//! holding exactly three `#[rudzio::test]` fns.
//!
//! # Historical symptom (pre-80ab61a)
//!
//! ```text
//! thread '<unnamed>' panicked at linkme-0.3.37/src/distributed_slice.rs:246:18:
//! unsafe precondition(s) violated: slice::from_raw_parts requires the
//! pointer to be aligned and non-null
//! ... static_slice<rudzio::token::Token> at rudzio/src/runner.rs:1453
//! thread caused non-unwinding panic. aborting.
//! ```
//!
//! Fix `80ab61a` ("mount integration tests for bin-only members (#8)") is an
//! ancestor of `e29add3`, so this faithful shape is GREEN at `e29add3` and at
//! every descendant (including HEAD). This spec pins that behavior: the
//! runner must exit 0 and report exactly `version_short_flag`,
//! `version_long_flag`, `help_flag`.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::current_git_head;
use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread;

/// The runner shim: declares the `cli` submodule and the `#[rudzio::main]`
/// entry point. The tests themselves live in `tests/cli.rs` and carry no
/// `#[rudzio::main]`.
const MAIN_SHIM_SRC: &str = "mod cli;\n\n#[rudzio::main]\nfn main() {}\n";

/// `tests/cli.rs` carrying exactly three `#[rudzio::test]` fns, no
/// `#[rudzio::main]`, driving the `futures::ThreadPool` runtime. Verbatim
/// from the operator's shape (fully-qualified suite/runtime/test types).
const CLI_TEST_SRC: &str = r#"//! Integration tests for the `modera` binary.

#[rudzio::suite([
    (runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test),
])]
mod tests {
    use std::process::Output;

    use anyhow::{Context as _, ensure};

    /// Run the `modera` binary with the given arguments and capture its output.
    fn run(args: &[&str]) -> anyhow::Result<Output> {
        std::process::Command::new(env!("CARGO_BIN_EXE_modera"))
            .args(args)
            .output()
            .context("failed to execute the modera binary")
    }

    #[rudzio::test]
    async fn version_short_flag(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {
        let output = run(&["-V"])?;
        ensure!(output.status.code() == Some(0_i32));
        Ok(())
    }

    #[rudzio::test]
    async fn version_long_flag(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {
        let output = run(&["--version"])?;
        ensure!(output.status.code() == Some(0_i32));
        Ok(())
    }

    #[rudzio::test]
    async fn help_flag(_ctx: &rudzio::common::context::Test) -> anyhow::Result<()> {
        let output = run(&["--help"])?;
        ensure!(output.status.code() == Some(0_i32));
        Ok(())
    }
}
"#;

/// A real, non-trivial `modera` bin that pulls `output` and `pico-args` and
/// handles `-V`/`--version`/`-h`/`--help` by printing and exiting 0.
const MODERA_MAIN_SRC: &str = r#"//! The `modera` command-line tool.

use std::process::ExitCode;

const HELP: &str = "modera - a tiny CLI\n\nUSAGE:\n  modera [-V|--version] [-h|--help] [NAME]\n";

fn main() -> ExitCode {
    let mut args = pico_args::Arguments::from_env();
    if args.contains(["-h", "--help"]) {
        output::print(HELP);
        return ExitCode::SUCCESS;
    }
    if args.contains(["-V", "--version"]) {
        output::print(concat!("modera ", env!("CARGO_PKG_VERSION")));
        return ExitCode::SUCCESS;
    }
    let name: Option<String> = match args.opt_free_from_str() {
        Ok(name) => name,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    output::print(&format!("hello, {}", name.as_deref().unwrap_or("world")));
    ExitCode::SUCCESS
}
"#;

/// The `output` lib crate: a tiny helper the `modera` bin calls.
const OUTPUT_LIB_SRC: &str = "//! Tiny output helper.\n\n/// Print a line to stdout.\npub fn print(s: &str) {\n    println!(\"{s}\");\n}\n";

/// The `.cargo/config.toml` the operator's workspace ships: a repo-relative
/// `build-dir`. The harness overrides it with `CARGO_BUILD_BUILD_DIR` so the
/// run stays isolated from the repo, but the file is materialised for
/// faithfulness.
const DOT_CARGO_CONFIG: &str = "[build]\nbuild-dir = \"tmp/build-dir\"\n";

/// Repo root = parent of `cargo-rudzio/` (the rudzio workspace).
fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
}

/// Cargo home that already holds cargo-rudzio's full dependency graph.
fn cargo_home_with_full_graph() -> PathBuf {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// Drive `cargo run -p cargo-rudzio -- test <args>` from the repo root with a
/// FRESH build.
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
        // The fixture declares `rudzio` as a `file://` git dep pinned to the
        // checkout's HEAD. `--offline` blocks cargo from resolving a git rev
        // not already present in the ambient git db cache (a fresh rev like
        // HEAD is never cached yet), so the git fetch must be permitted here.
        // `file://` is a local fetch (no network); the crates.io registry
        // deps are already cached.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Write the workspace root manifest: `members = ["crates/*"]`, resolver 2,
/// `[workspace.package] edition = "2024"`, `[workspace.lints.rust]` with
/// `warnings = "deny"`/`unsafe_code = "deny"`/`check-cfg`, panic-abort
/// profiles, and a `rudzio` **git** dependency (inherited via
/// `rudzio.workspace = true`) plus `output` (path) / `pico-args` (pinned
/// version) workspace deps.
fn write_workspace_manifest(root: &Path, git_rev: &str) -> io::Result<()> {
    let rudzio_root = repo_root();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"crates/*\"]\n\
             resolver = \"2\"\n\
             \n\
             [workspace.dependencies]\n\
             anyhow = \"=1.0.104\"\n\
             output = {{ path = \"crates/output\" }}\n\
             pico-args = \"=0.5.0\"\n\
             rudzio = {{ git = \"file://{rudzio}\", rev = \"{git_rev}\", features = [\"runtime-futures\", \"runtime-compio\", \"common\"] }}\n\
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
             panic = \"abort\"\n",
            rudzio = rudzio_root.display(),
            git_rev = git_rev,
        ),
    )
}

/// Write `modera`'s bin-only manifest: `[[bin]]` (no `[lib]`),
/// `autotests = false`, normal deps on `output` + `pico-args`, dev-deps on
/// `anyhow` + `rudzio` (both `workspace = true`), and a single
/// `[[test]] name = "main" path = "tests/main.rs" harness = false` shim.
fn write_modera_manifest(modera_dir: &Path) -> io::Result<()> {
    fs::write(
        modera_dir.join("Cargo.toml"),
        "[package]\n\
         name = \"modera\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         autotests = false\n\
         \n\
         [[bin]]\n\
         name = \"modera\"\n\
         path = \"src/main.rs\"\n\
         \n\
         [dependencies]\n\
         output = { workspace = true }\n\
         pico-args = { workspace = true }\n\
         \n\
         [dev-dependencies]\n\
         anyhow.workspace = true\n\
         rudzio.workspace = true\n\
         \n\
         [[test]]\n\
         name = \"main\"\n\
         path = \"tests/main.rs\"\n\
         harness = false\n\
         \n\
         [lints]\n\
         workspace = true\n",
    )
}

/// Write the `output` lib crate manifest (edition inherited, lints inherited).
fn write_output_manifest(output_dir: &Path) -> io::Result<()> {
    fs::write(
        output_dir.join("Cargo.toml"),
        "[package]\n\
         name = \"output\"\n\
         version = \"0.1.0\"\n\
         edition.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n",
    )
}

/// Materialise the operator's exact workspace at `root`: `crates/output` and
/// `crates/modera`, plus the workspace `.cargo/config.toml`.
fn write_modera_crate(root: &Path, git_rev: &str) -> io::Result<()> {
    let crates = root.join("crates");
    let output_dir = crates.join("output");
    let modera_dir = crates.join("modera");
    let output_src = output_dir.join("src");
    let modera_src = modera_dir.join("src");
    let modera_tests = modera_dir.join("tests");
    fs::create_dir_all(&output_src)?;
    fs::create_dir_all(&modera_src)?;
    fs::create_dir_all(&modera_tests)?;

    write_workspace_manifest(root, git_rev)?;
    write_output_manifest(&output_dir)?;
    write_modera_manifest(&modera_dir)?;

    fs::create_dir_all(root.join(".cargo"))?;
    fs::write(root.join(".cargo").join("config.toml"), DOT_CARGO_CONFIG)?;

    fs::write(output_src.join("lib.rs"), OUTPUT_LIB_SRC)?;
    fs::write(modera_src.join("main.rs"), MODERA_MAIN_SRC)?;
    fs::write(modera_tests.join("main.rs"), MAIN_SHIM_SRC)?;
    fs::write(modera_tests.join("cli.rs"), CLI_TEST_SRC)?;
    Ok(())
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Output;

    use super::{
        IsolatedCargoTest, cargo_home_with_full_graph, current_git_head, run_cargo_rudzio_fresh,
        write_modera_crate,
    };

    /// The three `#[rudzio::test]` fns the operator's `tests/cli.rs` declares.
    const EXPECTED_TESTS: [&str; 3] = ["version_short_flag", "version_long_flag", "help_flag"];

    /// Assert a successful `cargo rudzio test` run reported exactly the three
    /// `modera` tests.
    fn assert_three_tests(output: &Output) -> anyhow::Result<()> {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::ensure!(
            output.status.success(),
            "bin-only member `modera` must run its `tests/cli.rs` tests (exit 0), but the runner \
             exited {:?}.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status.code(),
        );
        for expected in EXPECTED_TESTS {
            anyhow::ensure!(
                stdout.contains(expected),
                "runner stdout must name the aggregated test `{expected}`.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            );
        }
        anyhow::ensure!(
            stdout.contains("3 passed"),
            "runner must report exactly 3 passing tests, but stdout did not contain \"3 passed\".\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
        Ok(())
    }

    /// Build a task-local cargo build dir beside the cargo target dir.
    fn cargo_build_dir_for(cargo_target_dir: &Path) -> anyhow::Result<PathBuf> {
        Ok(cargo_target_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir"))
    }

    /// **Bug #8 (operator's exact shape, `-p modera`)** — a bin-only workspace
    /// member whose tests are a `tests/cli.rs` submodule of a separate
    /// `tests/main.rs` shim, using a `futures::ThreadPool` runtime and a git
    /// rudzio dep, must aggregate exactly its three `#[rudzio::test]` fns.
    #[rudzio::test]
    async fn modera_exact_shape_package_filter_aggregates_all_tests(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("modera-exact");
        let git_rev = current_git_head()
            .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))?;
        write_modera_crate(&workspace, &git_rev)?;

        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let workspace_manifest = workspace.join("Cargo.toml").to_string_lossy().into_owned();
        let output = run_cargo_rudzio_fresh(
            &cargo_home_with_full_graph(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &["--manifest-path", &workspace_manifest, "-p", "modera"],
        )?;
        assert_three_tests(&output)
    }

    /// **Bug #8 (operator's exact shape, workspace-wide)** — the same workspace
    /// driven via `cargo rudzio test` (no package filter) must aggregate the
    /// `modera` tests and skip the lib-only `output` member.
    #[rudzio::test]
    async fn modera_exact_shape_workspace_wide_aggregates_all_tests(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = ctx.suite_tempdir().join("modera-exact-ws");
        let git_rev = current_git_head()
            .ok_or_else(|| anyhow::anyhow!("git HEAD must resolve for a faithful git-dep repro"))?;
        write_modera_crate(&workspace, &git_rev)?;

        let cargo_build_dir = cargo_build_dir_for(ctx.cargo_target_dir())?;
        let workspace_manifest = workspace.join("Cargo.toml").to_string_lossy().into_owned();
        let output = run_cargo_rudzio_fresh(
            &cargo_home_with_full_graph(),
            ctx.cargo_target_dir(),
            &cargo_build_dir,
            &["--manifest-path", &workspace_manifest],
        )?;
        assert_three_tests(&output)
    }
}
