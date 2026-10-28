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
use std::fs;
use std::io::{BufReader, Result as IoResult, Write as _, stderr, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio, abort};

use anyhow::{Context as _, Result, bail};
use cargo_metadata::{Message, TargetKind};
use cargo_rudzio::build_id;
use cargo_rudzio::cli::{
    RunnerExit, aggregator_cargo_args, extract_config_values, format_target_flag_warning,
    parse_test_args, runner_exit,
};
use cargo_rudzio::config_env;
use cargo_rudzio::coverage;
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
    --build-id                       Print the binary build identity
                                     (git HEAD, dirty state, version,
                                     stamp, profile) and exit.
    --version, -V                    Print the crate version and build
                                     identity, then exit.
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
        Some("--build-id" | "build-id") => {
            write_stdout(&format!("{}\n", build_id::line()));
            Ok(ExitCode::SUCCESS)
        }
        Some("--version" | "-V" | "version") => {
            write_stdout(&format!(
                "cargo-rudzio {}\n{}\n",
                build_id::VERSION,
                build_id::line()
            ));
            Ok(ExitCode::SUCCESS)
        }
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
    let argv = aggregator_cargo_args(&parsed, manifest_str, &panic_profile_overrides(&plan));
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    // Anchor the spawned cargo at the consumer's workspace root so cargo
    // resolves `.cargo/config.toml` from the same place a plain
    // `cargo test` in that workspace would. Config files are discovered
    // by walking up from the process CWD (independent of
    // `--manifest-path`); anchoring at the consumer's root keeps the
    // consumer's own `[profile]` (e.g. `[profile.dev] panic = "abort"`)
    // authoritative instead of letting a `[profile]` from cargo-rudzio's
    // own process directory override the re-emitted table (Bug #4).
    cmd.current_dir(plan.workspace_root.as_std_path());
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
    //
    // Coverage carve-out: under `cargo llvm-cov`'s `show-env` contract the
    // instrumented objects and the runner's `.profraw` dump must land where
    // `cargo llvm-cov report` searches (`CARGO_LLVM_COV_TARGET_DIR`), so the
    // inner build anchors there instead of the per-aggregator subdir. Fails
    // loudly on a partial coverage environment (marker set, target dir unset).
    let dirs = coverage::resolve_aggregator_dirs(
        coverage::is_active(),
        env::var_os(coverage::COVERAGE_TARGET_DIR_ENV).as_deref(),
        env::var_os(coverage::COVERAGE_BUILD_DIR_ENV).as_deref(),
        &target,
    )?;
    cmd.env("CARGO_TARGET_DIR", &dirs.target_dir);
    if let Some(build_dir) = &dirs.build_dir {
        cmd.env("CARGO_BUILD_BUILD_DIR", build_dir);
    }
    stamp_build_identity(&mut cmd);
    // Coverage part 2 (Bug #3): instrument the runner and remap the
    // bridge-local source copies (see `apply_coverage_build_env`).
    if coverage::is_active() {
        apply_coverage_build_env(&mut cmd, &plan)?;
    }
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
    // Coverage part 3 (Bug #3): expose the runner object to the report
    // (see `expose_runner_for_coverage`).
    if coverage::is_active() {
        expose_runner_for_coverage(&runner_path, &plan)?;
    }
    if parsed.no_run {
        // --no-run: same single cargo build, skip the exec step,
        // print the runner path so downstream tooling can pick it up.
        // Decisions Log [I9] USER-RATIFIED.
        write_stdout(&format!("{}\n", runner_path.display()));
        return Ok(ExitCode::SUCCESS);
    }
    // Direct exec: the runner is already built. Zero second cargo
    // subprocess. Decisions Log [C1] USER-RATIFIED.
    let cli_config = extract_config_values(&parsed.forwarded_cargo_args);
    let mut runner_cmd = runner_command(&runner_path, &parsed.runner_args, &plan, &cli_config)?;
    let runner_status = runner_cmd.status().with_context(|| {
        format!(
            "failed to exec runner at {runner_path}",
            runner_path = runner_path.display(),
        )
    })?;
    match runner_exit(runner_status.code()) {
        RunnerExit::Code(code) => Ok(ExitCode::from(code)),
        // Abort (signal-terminated runner) and any future [`RunnerExit`]
        // variant: propagate the abnormal termination by aborting so the
        // parent observes a signal-killed process instead of a clean
        // exit 1 (Bug #4).
        RunnerExit::Abort | _ => {
            abort();
        }
    }
}

/// Build the directly exec'd runner command: user runner args plus the
/// consumer's `[env]` config.
///
/// Resolves the workspace `.cargo/config` (extensionless preferred) +
/// `$CARGO_HOME/config` under cargo's precedence, then applies `--config`
/// overrides (`cli_config`) as the highest layer. Stock cargo sets these on
/// the test binary it runs; the direct exec skips cargo's injection, so
/// `std::env::var` on a `[env]` key would otherwise come back `NotPresent`
/// (compile-time `option_env!` still works because the inner `cargo build`
/// is anchored at the workspace root). Only the runner is patched: build
/// scripts, proc-macros, and rustc already receive `[env]` from that inner
/// cargo.
///
/// # Errors
///
/// Fails when a config file is unreadable or holds an unsupported `[env]`
/// entry.
fn runner_command(
    runner_path: &Path,
    runner_args: &[String],
    plan: &generate::Plan,
    cli_config: &[String],
) -> Result<Command> {
    let mut runner_cmd = Command::new(runner_path);
    runner_cmd.args(runner_args);
    for (name, var) in config_env::resolve(plan.workspace_root.as_std_path(), cli_config)? {
        // `force = true` overrides an already-set variable; otherwise match
        // cargo's default of not clobbering the inherited environment.
        if var.force || env::var_os(&name).is_none() {
            runner_cmd.env(&name, &var.value);
        }
    }
    Ok(runner_cmd)
}

/// Seed the child cargo build's git identity from this process's stamp.
///
/// Members compile from copies under `CARGO_TARGET_DIR` that have no
/// `.git`, so a member build script that stamps `git rev-parse HEAD`
/// (cargo-rudzio itself, in the dogfood) would otherwise resolve
/// `unknown` and fail the freshness guard.
fn stamp_build_identity(cmd: &mut Command) {
    cmd.env("CARGO_RUDZIO_BUILD_GIT_HEAD", build_id::GIT_HEAD);
    cmd.env("CARGO_RUDZIO_BUILD_GIT_DIRTY", build_id::GIT_DIRTY);
}

/// Instrument the runner and remap bridge source copies under coverage.
///
/// Under the `show-env` contract the rustc wrapper instruments a crate only
/// when its `--crate-name` is in `__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES`;
/// the runner and per-bin bridge libs are generated by cargo-rudzio and
/// absent from that list, so they are appended. The bridge-local source
/// copies are remapped back to the members' real directories with
/// `--remap-path-prefix`, so `cargo llvm-cov report` attributes line
/// coverage to the member `src/` files instead of the transient copies.
///
/// # Errors
///
/// Fails loudly when the crate-names variable is missing (a partial coverage
/// environment), rather than silently building an uninstrumented runner that
/// would emit no `.profraw`.
fn apply_coverage_build_env(cmd: &mut Command, plan: &generate::Plan) -> Result<()> {
    // A half-set coverage environment (e.g. `eval "$(cargo llvm-cov show-env)"`
    // without `--sh`, which leaves the assignments shell-local) would build an
    // uninstrumented runner that emits no `.profraw`. Fail loudly before
    // spawning the inner cargo rather than silently yielding an empty report.
    coverage::validate_wrapper_env()?;
    let existing_names = match env::var(coverage::WRAPPER_CRATE_NAMES_ENV) {
        Ok(names) => names,
        Err(source) => {
            return Err(anyhow::anyhow!(
                "`{}` is set but `{}` is not: the coverage environment is incomplete \
                 ({source}). Re-run `cargo llvm-cov show-env` and eval its output before \
                 `cargo rudzio test`.",
                coverage::COVERAGE_ENV,
                coverage::WRAPPER_CRATE_NAMES_ENV,
            ));
        }
    };
    cmd.env(
        coverage::WRAPPER_CRATE_NAMES_ENV,
        coverage::augment_crate_names(&existing_names, &plan.coverage_crate_names()),
    );
    let remap_flags: Vec<String> = plan
        .coverage_remap_prefixes()
        .iter()
        .map(|(from, to)| coverage::remap_prefix_flag(from, to))
        .collect::<Result<_>>()?;
    let mut rustflags = env::var(coverage::RUSTFLAGS_ENV).unwrap_or_default();
    if !remap_flags.is_empty() {
        if !rustflags.is_empty() {
            rustflags.push(' ');
        }
        rustflags.push_str(&remap_flags.join(" "));
    }
    cmd.env(coverage::RUSTFLAGS_ENV, &rustflags);
    Ok(())
}

/// Copy the instrumented runner to a member stem the report collects.
///
/// `cargo llvm-cov report` matches object stems against workspace member
/// names, and `rudzio_auto_runner` is not one, so the runner is invisible to
/// it. Copying it next to itself under the first member's underscored package
/// name makes the collector pick it up; the binary carries every member's
/// coverage (it links all bridge rlibs), so one copy suffices.
fn expose_runner_for_coverage(runner_path: &Path, plan: &generate::Plan) -> Result<()> {
    let member_stem = plan
        .coverage_object_name()
        .ok_or_else(|| anyhow::anyhow!("coverage requested but the plan has no members"))?;
    let exposed = runner_path.with_file_name(&member_stem);
    fs::copy(runner_path, &exposed).with_context(|| {
        format!(
            "copying instrumented runner {} -> {} so `cargo llvm-cov report` can collect it",
            runner_path.display(),
            exposed.display(),
        )
    })?;
    Ok(())
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
    let mut argv: Vec<String> = Vec::with_capacity(match rest.len().checked_add(1) {
        Some(c) => c,
        None => 0,
    });
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

/// Extract `--config profile.<p>.panic="<v>"` args from the plan's re-emitted
/// `[profile]` table.
///
/// Returns one `--config` arg per profile that declares a literal `panic`
/// value (`[profile.dev]`, `[profile.release]`, …). `package.*` /
/// `build-override` sub-tables are skipped: they are per-package or
/// build-script scopes, not profiles, and never carry a top-level `panic`.
///
/// `--config` is the highest-precedence layer in cargo's config hierarchy,
/// above every `.cargo/config.toml` the inner build discovers by walking up
/// from the consumer's workspace root. Re-stating the consumer's `panic`
/// there is what keeps it authoritative even when an ancestor config (the
/// rudzio repo's own `.cargo/config.toml`, which intentionally pins
/// `panic = "unwind"` for the framework's self-tests) would otherwise
/// override the aggregator manifest's re-emitted `panic = "abort"`.
fn panic_profile_overrides(plan: &generate::Plan) -> Vec<String> {
    let Some(profile) = &plan.profile else {
        return Vec::new();
    };
    let mut args = Vec::new();
    for (name, item) in profile {
        let Some(table) = item.as_table() else {
            continue;
        };
        let Some(value) = table.get("panic").and_then(|entry| entry.as_str()) else {
            continue;
        };
        args.push(format!("--config=profile.{name}.panic=\"{value}\""));
    }
    args
}
