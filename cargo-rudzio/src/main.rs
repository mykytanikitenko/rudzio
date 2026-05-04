//! `cargo-rudzio` subcommand binary.
//!
//! Wraps three operations behind a single cargo subcommand: generate the
//! aggregator crate (`generate-runner`), generate-and-run the aggregator
//! (`test`), and forward to `rudzio-migrate`.

#![allow(
    unused_results,
    clippy::needless_pass_by_value,
    reason = "toml_edit's insert/push API routinely returns the previous value; CLI glue does not care about the dropped option"
)]

use std::env;
use std::io::{Result as IoResult, Write as _, stderr, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context as _, Result, bail};
use cargo_rudzio::cli::parse_test_args;
use cargo_rudzio::{generate, spawn_env};
use rudzio_migrate::run::entry_with_args as rudzio_migrate_entry;

/// Top-level usage string printed for `--help` / unknown subcommands.
const USAGE: &str = "\
cargo-rudzio - Cargo subcommand: single-binary test aggregation + rudzio-migrate

USAGE:
    cargo rudzio <COMMAND> [ARGS...]

COMMANDS:
    test [SELECTORS...] [ARGS...]    Build every rudzio test in the workspace
                                     into ONE binary (grouped by runtime and
                                     suite) and run it.

                                     SELECTORS narrow the aggregator to a
                                     subset of workspace members BEFORE the
                                     binary is built (cheaper than filtering
                                     at runtime):
                                       -p, --package <NAME>  exact Cargo
                                         package-name match. Repeatable.
                                       --exclude <NAME>  exact Cargo
                                         package-name match for members to
                                         drop. Repeatable. Combine with -p
                                         to keep all-but-N, or use alone to
                                         skip noisy crates from the
                                         workspace-wide default.
                                       <PATH>  any positional argument that
                                         resolves to a directory on disk
                                         restricts the aggregator to members
                                         at-or-under that path. Repeatable.

                                     Anything else in ARGS forwards verbatim
                                     to the rudzio runner (positional filter,
                                     --skip, --output=plain, etc.). Run the
                                     binary with --help to see runner flags.
    migrate [ARGS...]                Run rudzio-migrate (converts stock cargo
                                     tests to rudzio). ARGS are forwarded
                                     verbatim. See `cargo rudzio migrate --help`.
    generate-runner [--output DIR]   Generate the aggregator crate at DIR
                                     without running it (default:
                                     <target-dir>/rudzio-auto-runner).
    help, --help, -h                 Print this message.

EXAMPLES:
    Run only one workspace member's rudzio tests:
        cargo rudzio test -p rudzio-migrate

    Combine package selection with a runner filter:
        cargo rudzio test -p rudzio-migrate my_failing_test

    Pipe-safe output for an AI agent or log shipper:
        cargo rudzio test -- --output=plain --color=never
";

/// Write `text` to stdout, ignoring any I/O error.
fn write_stdout(text: &str) {
    let _io_result: IoResult<()> = stdout().lock().write_all(text.as_bytes());
}

/// Write `text` to stderr, ignoring any I/O error.
fn write_stderr(text: &str) {
    let _io_result: IoResult<()> = stderr().lock().write_all(text.as_bytes());
}

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().collect();
    match dispatch(&argv) {
        Ok(code) => code,
        Err(err) => {
            write_stderr(&format!("cargo-rudzio: {err:#}\n"));
            ExitCode::from(1)
        }
    }
}

/// Match the user-supplied argv against the command grammar and dispatch to one of the per-subcommand handlers.
fn dispatch(argv: &[String]) -> Result<ExitCode> {
    let mut walker = argv.iter().skip(1);
    let first = walker.next().map(String::as_str);
    let subcommand = match first {
        Some("rudzio") => walker.next().map(String::as_str),
        other => other,
    };
    let rest: Vec<String> = walker.cloned().collect();
    match subcommand {
        None | Some("help" | "--help" | "-h") => {
            write_stdout(USAGE);
            Ok(ExitCode::SUCCESS)
        }
        Some("generate-runner") => run_generate(&rest).map(|()| ExitCode::SUCCESS),
        Some("test") => run_test(&rest),
        Some("migrate") => Ok(run_migrate(&rest)),
        Some(cmd) => {
            write_stderr(&format!(
                "cargo-rudzio: unknown subcommand `{cmd}`\n\n{USAGE}"
            ));
            Ok(ExitCode::from(2))
        }
    }
}

/// Handler for `cargo rudzio generate-runner`: produce an aggregator crate at the requested output directory.
fn run_generate(rest: &[String]) -> Result<()> {
    let output = parse_output_flag(rest)?;
    let plan = generate::plan_from_cwd()?;
    emit_diagnostic_warnings(&plan);
    let target = output.unwrap_or_else(|| plan.default_output_dir());
    generate::write_runner(&plan, &target)?;
    write_stdout(&format!(
        "cargo-rudzio: generated aggregator at {}\n",
        target.display()
    ));
    Ok(())
}

/// Handler for `cargo rudzio test`: generate the aggregator crate then `cargo run` it with the user's runner args.
fn run_test(rest: &[String]) -> Result<ExitCode> {
    let parsed = parse_test_args(rest, Path::is_dir)?;
    let mut plan = generate::plan_from_cwd()?;
    if !parsed.filters.include_paths.is_empty() {
        plan.restrict_to_paths(&parsed.filters.include_paths)?;
    }
    if !parsed.filters.include_packages.is_empty() {
        plan.restrict_to_packages(&parsed.filters.include_packages)?;
    }
    plan.exclude_packages(&parsed.filters.exclude_packages)?;
    emit_diagnostic_warnings(&plan);
    let target = plan.default_output_dir();
    generate::write_runner(&plan, &target)?;
    let manifest = target.join("Cargo.toml");
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    for (key, value) in &spawn_env() {
        cmd.env(key, value);
    }
    let status = cmd
        .arg("run")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--")
        .args(&parsed.runner_args)
        .status()
        .with_context(|| {
            format!(
                "failed to spawn cargo run --manifest-path {}",
                manifest.display()
            )
        })?;
    Ok(status.code().map_or_else(
        || ExitCode::from(1),
        |code| ExitCode::from(u8::try_from(code & 0xFF_i32).unwrap_or(1)),
    ))
}

/// Print warnings from the src-scan diagnostic pass before the
/// aggregator build kicks off. Warnings are advisory — we don't block
/// the build on them, because silent failure is worse than visible
/// warnings for the rare false-positive case.
fn emit_diagnostic_warnings(plan: &generate::Plan) {
    for warning in generate::scan_unbroadened_cfg_test_mods_in_plan(plan) {
        write_stderr(&format!("warning: {warning}\n"));
    }
}

/// Handler for `cargo rudzio migrate`: forward every arg verbatim into `rudzio_migrate::run::entry_with_args`.
fn run_migrate(rest: &[String]) -> ExitCode {
    let mut argv: Vec<String> = Vec::with_capacity(rest.len().saturating_add(1));
    argv.push("rudzio-migrate".to_owned());
    argv.extend(rest.iter().cloned());
    rudzio_migrate_entry(argv)
}

/// Parse the `--output DIR` / `-o DIR` / `--output=DIR` flag from the args list.
fn parse_output_flag(rest: &[String]) -> Result<Option<PathBuf>> {
    let mut out: Option<PathBuf> = None;
    let mut idx = 0;
    while let Some(arg) = rest.get(idx) {
        if arg == "--output" || arg == "-o" {
            let next_idx = idx.saturating_add(1);
            let val = rest
                .get(next_idx)
                .ok_or_else(|| anyhow::anyhow!("`{arg}` requires a value"))?;
            out = Some(PathBuf::from(val));
            idx = idx.saturating_add(2);
        } else if let Some(val) = arg.strip_prefix("--output=") {
            out = Some(PathBuf::from(val));
            idx = idx.saturating_add(1);
        } else {
            bail!("unexpected argument: `{arg}`");
        }
    }
    Ok(out)
}
