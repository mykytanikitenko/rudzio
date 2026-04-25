#![allow(
    unused_results,
    clippy::needless_pass_by_value,
    reason = "toml_edit's insert/push API routinely returns the previous value; CLI glue does not care about the dropped option"
)]

mod generate;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context as _, Result, bail};

const USAGE: &str = "\
cargo-rudzio - Cargo subcommand: single-binary test aggregation + rudzio-migrate

USAGE:
    cargo rudzio <COMMAND> [ARGS...]

COMMANDS:
    test [ARGS...]                   Build every rudzio test in the workspace
                                     into ONE binary (grouped by runtime and
                                     suite) and run it. ARGS are forwarded to
                                     the runner (filter patterns, --skip, etc.
                                     — the aggregator accepts rudzio's full
                                     config flag set).
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
    let target = output.unwrap_or_else(|| plan.default_output_dir());
    generate::write_runner(&plan, &target)?;
    println!(
        "cargo-rudzio: generated aggregator at {}",
        target.display()
    );
    Ok(())
}

fn run_test(rest: &[String]) -> Result<ExitCode> {
    let plan = generate::plan_from_cwd()?;
    let target = plan.default_output_dir();
    generate::write_runner(&plan, &target)?;
    let manifest = target.join("Cargo.toml");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("run")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--")
        .args(rest)
        .status()
        .with_context(|| format!("failed to spawn cargo run --manifest-path {}", manifest.display()))?;
    Ok(match status.code() {
        Some(code) => ExitCode::from(u8::try_from(code & 0xFF).unwrap_or(1)),
        None => ExitCode::from(1),
    })
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
