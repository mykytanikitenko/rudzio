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
use std::io::{BufReader, Result as IoResult, Write as _, stderr, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context as _, Result, bail};
use cargo_metadata::{Message, TargetKind};
use cargo_rudzio::cli::{aggregator_cargo_args, format_target_flag_warning, parse_test_args};
use cargo_rudzio::generate;
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

                                     CARGO-COMPAT FLAGS consumed locally:
                                       --manifest-path <PATH>  point
                                         cargo-rudzio at a workspace
                                         other than the current
                                         directory's. Repeated use is
                                         last-wins (mirrors cargo).
                                       --no-run  build the aggregator
                                         binary but skip running it. Cargo
                                         build output is in
                                         json-render-diagnostics format on
                                         stdout, so tooling can extract the
                                         binary path with
                                         `jq -r 'select(.executable != null) | .executable'`.
                                       --workspace, --all  silently
                                         accepted; rudzio always operates on
                                         the whole workspace by default.
                                       --nocapture, --show-output  silently
                                         accepted; rudzio's structured
                                         output already surfaces test
                                         stdout/stderr.
                                       --no-fail-fast  silently accepted;
                                         the rudzio runner already keeps
                                         going on a per-test failure.
                                       --lib, --bins, --bin <NAME>,
                                       --example <NAME>, --examples,
                                       --test <NAME>, --tests,
                                       --bench <NAME>, --benches,
                                       --all-targets, --doc  consumed
                                         with one consolidated stderr
                                         warning. The aggregator is one
                                         binary, so per-target selection
                                         has no rudzio analog.

                                     CARGO BUILD-GRAPH FLAGS forwarded to
                                     the spawned `cargo run` before `--`
                                     (so they shape the build, not the
                                     runner):
                                       --release, -r (short for --release),
                                       --profile <NAME>,
                                       --features <FEATURES>,
                                       -F <FEATURES> (short for --features),
                                       --all-features,
                                       --no-default-features,
                                       --target <TRIPLE> (repeatable),
                                       --target-dir <DIR>,
                                       --lockfile-path <PATH>,
                                       -j/--jobs <N>, --keep-going,
                                       --frozen, --locked, --offline,
                                       -v/--verbose (repeatable),
                                       -q/--quiet, --message-format <FMT>,
                                       --config <K=V> (repeatable),
                                       -Z <FLAG> (repeatable, nightly),
                                       --ignore-rust-version, --unit-graph,
                                       --future-incompat-report.
                                     Both space (`--features ci`) and
                                     equals (`--features=ci`) forms are
                                     accepted; the user's spelling is
                                     forwarded unchanged.

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

    Build the aggregator without running it; print the binary path:
        cargo rudzio test --no-run \\
          | jq -r 'select(.executable != null) | .executable'
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

/// Handler for `cargo rudzio test`: generate the aggregator crate,
/// run ONE `cargo build --workspace --bins
/// --message-format=json-render-diagnostics`, parse the JSON stream
/// to find the rudzio-auto-runner artifact path, then direct-exec
/// that binary with the user's runner args (or print the path under
/// `--no-run`).
///
/// This is the Option B-json orchestration per PLAN.md Decisions Log
/// [C1]: exactly ONE cargo subprocess per invocation. No nested cargo
/// from build.rs. No `cargo run` wrapper (which would not build
/// sibling workspace bins). The runner path comes from cargo's
/// authoritative `compiler-artifact` JSON message — robust against
/// any future change in cargo's output layout convention.
fn run_test(rest: &[String]) -> Result<ExitCode> {
    let parsed = parse_test_args(rest, Path::is_dir)?;
    if let Some(warning) = format_target_flag_warning(&parsed.ignored_target_flags) {
        write_stderr(&format!("warning: {warning}\n"));
    }
    let mut plan = match parsed.manifest_path.as_deref() {
        Some(manifest) => generate::plan_from_manifest(manifest)?,
        None => generate::plan_from_cwd()?,
    };
    if !parsed.filters.include_paths.is_empty() {
        plan.restrict_to_paths(&parsed.filters.include_paths)?;
    }
    if !parsed.filters.include_packages.is_empty() {
        plan.restrict_to_packages(&parsed.filters.include_packages)?;
    }
    plan.exclude_packages(&parsed.filters.exclude_packages)?;
    // Bug #3: `--test <name>` filters the per-binary set so unrelated
    // (potentially broken) binaries aren't compiled into the aggregator.
    plan.restrict_to_test_binaries(&parsed.filters.include_test_binaries)?;
    emit_diagnostic_warnings(&plan);
    let target = plan.default_output_dir();
    generate::write_runner(&plan, &target)?;
    let manifest = target.join("Cargo.toml");
    let manifest_str = manifest.to_str().ok_or_else(|| {
        anyhow::anyhow!("manifest path is not valid UTF-8: {}", manifest.display())
    })?;
    let argv = aggregator_cargo_args(&parsed, manifest_str);
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    // Pin the spawned cargo's `CARGO_TARGET_DIR` AND
    // `CARGO_BUILD_BUILD_DIR` to per-aggregator subdirs under the
    // resolved target dir. Three reasons:
    //
    // 1. Concurrent `cargo rudzio test` invocations against DIFFERENT
    //    consumer workspaces that share one outer `CARGO_TARGET_DIR`
    //    must not race on the aggregator binary path. Without this
    //    override, every invocation's spawned cargo writes
    //    `<user-target>/debug/rudzio-auto-runner` -- last-writer-wins
    //    on the binary, and the early invocation's still-running
    //    binary gets overwritten mid-execution. With the override,
    //    each aggregator gets its own `target/` and its own binary.
    //
    // 2. The same isolation must extend to `build-dir`. Cargo
    //    1.92+'s `[build] build-dir = "..."` shares compiled
    //    build-scripts across all crates in `build-dir` keyed on
    //    `(crate-name, crate-version, package-source-id)`. Two
    //    aggregators called `rudzio-auto-runner v0.0.0` with
    //    different paths can hash to the same package-source-id key
    //    when their generated build.rs has byte-identical inputs the
    //    fingerprinter happens to consider, causing one aggregator's
    //    compiled build-script to be REUSED for the other
    //    aggregator's invocation -- which then emits
    //    `cargo:rustc-env=RUDZIO_MEMBER_MANIFEST_DIR_consumer=
    //    <path-from-the-OTHER-aggregator>` and dies on
    //    `manifest path .../consumer/Cargo.toml does not exist`.
    //    Per-aggregator `CARGO_BUILD_BUILD_DIR` eliminates that
    //    cross-talk by giving each aggregator its own compile cache.
    //
    // 3. Respects the user's `CARGO_TARGET_DIR` choice as the ROOT:
    //    we don't `env_remove` it, we anchor under it. Whatever
    //    storage strategy the user picked (external SSD, tmpfs cache,
    //    CI mount) keeps applying to rudzio's artefacts.
    //
    // The aggregator subdir name itself encodes a consumer-workspace
    // hash (see `Plan::aggregator_subdir_name`), so distinct
    // consumers land in distinct top-level dirs.
    cmd.env("CARGO_TARGET_DIR", target.join("target"));
    cmd.env("CARGO_BUILD_BUILD_DIR", target.join("build-dir"));
    cmd.args(&argv);
    // Capture stdout so we can parse the cargo JSON message stream.
    // stderr stays inherited so cargo's rendered human-friendly
    // diagnostics (from `--message-format=json-render-diagnostics`)
    // still reach the user's terminal directly.
    cmd.stdout(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn cargo with args {argv:?}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("cargo build subprocess stdout was not captured"))?;
    let reader = BufReader::new(stdout);
    // Stream the JSON message lines as cargo emits them. We're
    // looking for the `compiler-artifact` message describing the
    // rudzio-auto-runner bin executable. Cargo emits multiple
    // artifact messages per build (one per crate, possibly several
    // per crate for split DWARF / dep-info). The runner shows up as
    // an artifact with `target.name == "rudzio-auto-runner"` AND
    // `target.kind` containing `Bin`. Its `executable` field holds
    // the absolute path of the binary on disk.
    //
    // Iterating to exhaustion (rather than breaking on first match)
    // keeps the pipe drained so cargo never blocks on a full stdout
    // buffer — critical for large workspaces with many artifacts.
    let mut found_runner_path: Option<PathBuf> = None;
    for parsed_msg in Message::parse_stream(reader) {
        let cargo_msg = parsed_msg.with_context(|| "failed to parse cargo JSON message stream")?;
        if let Message::CompilerArtifact(artifact) = cargo_msg
            && artifact.target.name == "rudzio-auto-runner"
            && artifact
                .target
                .kind
                .iter()
                .any(|kind| matches!(kind, TargetKind::Bin))
            && let Some(exe) = artifact.executable
        {
            found_runner_path = Some(exe.into_std_path_buf());
        }
    }
    let status = child
        .wait()
        .with_context(|| "failed to wait on cargo build subprocess")?;
    if !status.success() {
        return Ok(status.code().map_or_else(
            || ExitCode::from(1),
            |code| ExitCode::from(u8::try_from(code & 0xFF_i32).unwrap_or(1)),
        ));
    }
    let runner_path = found_runner_path.ok_or_else(|| {
        anyhow::anyhow!(
            "cargo build succeeded but emitted no `compiler-artifact` JSON message for the \
             rudzio-auto-runner bin; cannot determine the runner path for direct exec"
        )
    })?;
    if parsed.no_run {
        // --no-run: same single cargo build, skip the exec step,
        // print the runner path so downstream tooling can pick it up.
        // Decisions Log [I9] USER-RATIFIED.
        write_stdout(&format!("{}\n", runner_path.display()));
        return Ok(ExitCode::SUCCESS);
    }
    // Direct exec: the runner is already built. Zero second cargo
    // subprocess. Decisions Log [C1] USER-RATIFIED.
    let mut runner_cmd = Command::new(&runner_path);
    runner_cmd.args(&parsed.runner_args);
    let runner_status = runner_cmd.status().with_context(|| {
        format!(
            "failed to exec runner at {runner_path}",
            runner_path = runner_path.display(),
        )
    })?;
    Ok(runner_status.code().map_or_else(
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
    // Decisions Log [E7]: proc-macro members are skipped during
    // bridge generation; surface a per-member warning so users know
    // their proc-macro test surface is not aggregated.
    for warning in generate::scan_proc_macro_members_in_plan(plan) {
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
