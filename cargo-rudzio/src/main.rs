#![allow(
    unused_results,
    clippy::needless_pass_by_value,
    reason = "toml_edit's insert/push API routinely returns the previous value; CLI glue does not care about the dropped option"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context as _, Result, bail};
use cargo_rudzio::{generate, spawn_env};

const USAGE: &str = "\
cargo-rudzio - Cargo subcommand: single-binary test aggregation + rudzio-migrate

USAGE:
    cargo rudzio <COMMAND> [ARGS...]

COMMANDS:
    test [ARGS...]                   Build every rudzio test in the workspace
                                     into ONE binary (grouped by runtime and
                                     suite) and run it. ARGS forward to the
                                     runner (filter patterns, --skip, etc.).
    migrate [ARGS...]                Run rudzio-migrate (converts stock cargo
                                     tests to rudzio). ARGS are forwarded
                                     verbatim. See `cargo rudzio migrate --help`.
    generate-runner [--output DIR]   Generate the aggregator crate at DIR
                                     without running it (default:
                                     <target-dir>/rudzio-auto-runner).
    help, --help, -h                 Print this message.
";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    match dispatch(&argv) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cargo-rudzio: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(argv: &[String]) -> Result<ExitCode> {
    let mut args = argv.iter().skip(1);
    let first = args.next().map(String::as_str);
    let subcommand = match first {
        Some("rudzio") => args.next().map(String::as_str),
        other => other,
    };
    let rest: Vec<String> = args.cloned().collect();
    match subcommand {
        None | Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some("generate-runner") => run_generate(&rest).map(|()| ExitCode::SUCCESS),
        Some("test") => run_test(&rest),
        Some("migrate") => Ok(run_migrate(&rest)),
        Some(cmd) => {
            eprint!("cargo-rudzio: unknown subcommand `{cmd}`\n\n{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

fn run_generate(rest: &[String]) -> Result<()> {
    let output = parse_output_flag(rest)?;
    let plan = generate::plan_from_cwd()?;
    emit_diagnostic_warnings(&plan);
    let target = output.unwrap_or_else(|| plan.default_output_dir());
    generate::write_runner(&plan, &target)?;
    println!("cargo-rudzio: generated aggregator at {}", target.display());
    Ok(())
}

fn run_test(rest: &[String]) -> Result<ExitCode> {
    let (path_args, runner_args) = split_path_args(rest);
    let mut plan = generate::plan_from_cwd()?;
    if !path_args.is_empty() {
        plan.restrict_to_paths(&path_args)?;
    }
    emit_diagnostic_warnings(&plan);
    let target = plan.default_output_dir();
    generate::write_runner(&plan, &target)?;
    let manifest = target.join("Cargo.toml");
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    for (key, value) in &spawn_env() {
        cmd.env(key, value);
    }
    let status = cmd
        .arg("run")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--")
        .args(&runner_args)
        .status()
        .with_context(|| {
            format!(
                "failed to spawn cargo run --manifest-path {}",
                manifest.display()
            )
        })?;
    Ok(match status.code() {
        Some(code) => ExitCode::from(u8::try_from(code & 0xFF).unwrap_or(1)),
        None => ExitCode::from(1),
    })
}

/// Split positional `cargo rudzio test` args into directory paths
/// (used to restrict the aggregator to a subset of workspace members)
/// and runner-bound args (filters, --skip, etc., forwarded verbatim).
///
/// An arg counts as a path iff it is path-shaped (starts with `./`,
/// `../`, `/`, or is exactly `.` / `..`) AND resolves to a directory
/// on disk. The disk check guards against runner filters that happen
/// to look path-shaped — rudzio test names use `::`, so a real path
/// arg practically must exist as a directory at the time of the run.
fn split_path_args(rest: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let mut paths = Vec::new();
    let mut runner = Vec::new();
    for arg in rest {
        if is_existing_dir_path_arg(arg) {
            paths.push(PathBuf::from(arg));
        } else {
            runner.push(arg.clone());
        }
    }
    (paths, runner)
}

fn is_existing_dir_path_arg(s: &str) -> bool {
    let path_shaped =
        s == "." || s == ".." || s.starts_with("./") || s.starts_with("../") || s.starts_with('/');
    path_shaped && Path::new(s).is_dir()
}

/// Print warnings from the src-scan diagnostic pass before the
/// aggregator build kicks off. Warnings are advisory — we don't block
/// the build on them, because silent failure is worse than visible
/// warnings for the rare false-positive case.
fn emit_diagnostic_warnings(plan: &generate::Plan) {
    for w in generate::scan_unbroadened_cfg_test_mods_in_plan(plan) {
        eprintln!("warning: {w}");
    }
}

fn run_migrate(rest: &[String]) -> ExitCode {
    let mut argv: Vec<String> = Vec::with_capacity(rest.len() + 1);
    argv.push("rudzio-migrate".to_owned());
    argv.extend(rest.iter().cloned());
    rudzio_migrate::run::entry_with_args(argv)
}

fn parse_output_flag(rest: &[String]) -> Result<Option<PathBuf>> {
    let mut out: Option<PathBuf> = None;
    let mut idx = 0;
    while idx < rest.len() {
        let arg = &rest[idx];
        if arg == "--output" || arg == "-o" {
            let val = rest
                .get(idx + 1)
                .ok_or_else(|| anyhow::anyhow!("`{arg}` requires a value"))?;
            out = Some(PathBuf::from(val));
            idx += 2;
        } else if let Some(val) = arg.strip_prefix("--output=") {
            out = Some(PathBuf::from(val));
            idx += 1;
        } else {
            bail!("unexpected argument: `{arg}`");
        }
    }
    Ok(out)
}
