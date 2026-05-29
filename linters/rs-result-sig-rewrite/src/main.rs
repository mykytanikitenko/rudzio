//! `rs-result-sig-rewrite` CLI shell.
//!
//! Thin wrapper around `rs_result_sig_rewrite::transform`. Parses
//! flags, reads the target file, runs the transformer, emits either a
//! diff (default `--dry-run`) or writes the result back when the user
//! pairs `--write` with `--confirm-write`.
//!
//! Exit codes:
//! - `0` — transformation produced output (or nothing to do).
//! - `1` — argument or IO error, or `--write` without
//!   `--confirm-write`, or unknown flag.
//! - `2` — `syn` could not parse the source file.
//! - `3` — transformer encountered an unsupported case (currently
//!   only `const fn`) that requires human review.

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use rs_result_sig_rewrite::{TransformOutcome, render_diff, transform};

/// Parsed command-line flags.
struct Cli {
    file: String,
    dry_run: bool,
    write: bool,
    confirm_write: bool,
}

impl Cli {
    const fn empty() -> Self {
        Self {
            file: String::new(),
            dry_run: true,
            write: false,
            confirm_write: false,
        }
    }
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::empty();
    let mut idx = 0;
    while idx < args.len() {
        let Some(arg) = args.get(idx) else { break };
        match arg.as_str() {
            "--file" => {
                let value_idx = idx.saturating_add(1);
                let Some(value) = args.get(value_idx) else {
                    return Err("--file requires a value".to_owned());
                };
                cli.file = value.clone();
                idx = value_idx;
            }
            "--dry-run" => cli.dry_run = true,
            "--write" => {
                cli.write = true;
                cli.dry_run = false;
            }
            "--confirm-write" => cli.confirm_write = true,
            other => return Err(format!("unknown flag {other:?}")),
        }
        idx = idx.saturating_add(1);
    }
    if cli.file.is_empty() {
        return Err("--file <PATH> is required".to_owned());
    }
    if !Path::new(&cli.file).is_absolute() {
        return Err(format!(
            "--file requires an absolute path, got {path:?}",
            path = cli.file,
        ));
    }
    if cli.write && !cli.confirm_write {
        return Err("--write requires --confirm-write".to_owned());
    }
    Ok(cli)
}

fn run() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let cli = match parse_cli(&argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rs-result-sig-rewrite: {e}");
            return ExitCode::from(1);
        }
    };

    eprintln!("processing {file}", file = cli.file);

    let source = match fs::read_to_string(&cli.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rs-result-sig-rewrite: read failed: {path}: {e}", path = cli.file);
            return ExitCode::from(1);
        }
    };

    let outcome: TransformOutcome = match transform(&source) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rs-result-sig-rewrite: {e}");
            return ExitCode::from(2);
        }
    };

    for line in &outcome.logs {
        eprintln!("{line}");
    }

    if cli.write {
        if outcome.new_source == source {
            eprintln!("rs-result-sig-rewrite: no changes; file untouched");
        } else if let Err(e) = fs::write(&cli.file, &outcome.new_source) {
            eprintln!("rs-result-sig-rewrite: write failed: {path}: {e}", path = cli.file);
            return ExitCode::from(1);
        }
    } else {
        // Dry-run: emit the diff to stdout.
        let diff = render_diff(&source, &outcome.new_source);
        print!("{diff}");
    }

    eprintln!(
        "rs-result-sig-rewrite: {t} fns transformed, {s} skipped, {h} hazard TODOs inserted, {c} FIXME callsites flagged",
        t = outcome.transformed,
        s = outcome.skipped,
        h = outcome.hazard_todos,
        c = outcome.fixme_callsites,
    );

    ExitCode::from(outcome.exit_code)
}

fn main() -> ExitCode {
    run()
}
