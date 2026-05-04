//! Cargo-style command-line parsing for `cargo rudzio test`.
//!
//! Lives in the library (rather than `main.rs`) so integration tests can
//! drive each parser directly with synthetic argv slices and assert the
//! consumed-vs-forwarded split is correct for every flag spelling.
//!
//! The composition entry point is [`parse_test_args`]: it threads the
//! per-flag parsers in a fixed order and returns a structured result so
//! `run_test` can wire selectors into the `Plan` without re-parsing.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Filters applied to the aggregator's `Plan` BEFORE the binary is built.
///
/// Field order encodes the application order in `run_test`: paths
/// restrict first, packages narrow further, excludes drop last.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct PlanFilters {
    /// Cargo package names to drop from the workspace member set
    /// (`--exclude <NAME>`, exact match, repeatable).
    pub exclude_packages: Vec<String>,
    /// Cargo package names to keep in the aggregator's member set
    /// (`-p`/`--package <NAME>`, exact match, repeatable).
    pub include_packages: Vec<String>,
    /// Path roots; only members whose manifest dir sits under one of
    /// these survive (positional path arg that resolves to a directory).
    pub include_paths: Vec<PathBuf>,
}

impl PlanFilters {
    /// Empty filter set — every workspace member passes through.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            exclude_packages: Vec::new(),
            include_packages: Vec::new(),
            include_paths: Vec::new(),
        }
    }
}

/// Structured result of parsing a `cargo rudzio test` argv tail.
///
/// Splits the input into Plan filters (consumed locally to narrow the
/// aggregator) and runner args (forwarded verbatim to the rudzio test
/// binary after `--`).
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ParsedTestArgs {
    /// Plan-shaping selectors consumed by `cargo-rudzio` itself.
    pub filters: PlanFilters,
    /// Cargo target-selection flags consumed but not honoured. The
    /// aggregator is one binary, so per-target selection has no
    /// rudzio analog. The caller emits one consolidated stderr warning
    /// listing these so the user knows their flag was a no-op rather
    /// than silently honoured.
    pub ignored_target_flags: Vec<String>,
    /// `--no-run` was passed: build the aggregator binary but skip
    /// running it (mirrors `cargo test --no-run`).
    pub no_run: bool,
    /// Args forwarded verbatim to the rudzio runner after `--`.
    pub runner_args: Vec<String>,
}

impl ParsedTestArgs {
    /// Empty parse result — equivalent to passing no args at all.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            filters: PlanFilters::empty(),
            ignored_target_flags: Vec::new(),
            no_run: false,
            runner_args: Vec::new(),
        }
    }
}

/// Build the argv we hand to `cargo` for the aggregator invocation.
///
/// Default (`no_run = false`): `cargo run --manifest-path X -- <runner args>`.
/// `--no-run`: `cargo build --manifest-path X --message-format=json-render-diagnostics`,
/// dropping the `--` separator and any runner args (the aggregator
/// won't be spawned, so forwarding them is dead weight). The
/// `json-render-diagnostics` format keeps human-friendly diagnostics
/// on stderr while emitting machine-readable artifact records on
/// stdout, so downstream tooling (or AI agents) can extract the
/// built binary path with `jq -r 'select(.executable != null) | .executable'`.
#[inline]
#[must_use]
pub fn aggregator_cargo_args(parsed: &ParsedTestArgs, manifest: &str) -> Vec<String> {
    let mut argv = Vec::with_capacity(parsed.runner_args.len().saturating_add(5_usize));
    if parsed.no_run {
        argv.push("build".to_owned());
        argv.push("--manifest-path".to_owned());
        argv.push(manifest.to_owned());
        argv.push("--message-format=json-render-diagnostics".to_owned());
    } else {
        argv.push("run".to_owned());
        argv.push("--manifest-path".to_owned());
        argv.push(manifest.to_owned());
        argv.push("--".to_owned());
        argv.extend(parsed.runner_args.iter().cloned());
    }
    argv
}

/// Pull `--exclude <name>` / `--exclude=<name>` out of `args`.
///
/// Returns the collected package names and the args list with those
/// entries removed. Mirrors cargo's own `--exclude` semantics:
/// repeatable, takes one name per occurrence, name match is exact
/// against the Cargo package name (hyphenated form). No short form
/// (cargo doesn't define one).
///
/// Without this consumption, `--exclude` would land in the
/// aggregator's argv where the rudzio runner would warn about an
/// unrecognised flag and treat the package name as a positional
/// substring filter that almost never matches a fully-qualified test.
///
/// # Errors
///
/// Returns an error when `--exclude` is the last arg with no
/// following value, or when the equals form supplies an empty name.
#[inline]
pub fn parse_exclude_filters(args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut excluded = Vec::new();
    let mut remaining = Vec::new();
    let mut idx = 0_usize;
    while let Some(arg) = args.get(idx) {
        if arg == "--exclude" {
            let next_idx = idx.saturating_add(1_usize);
            let value = args
                .get(next_idx)
                .ok_or_else(|| anyhow::anyhow!("`{arg}` requires a package name"))?;
            excluded.push(value.clone());
            idx = next_idx.saturating_add(1_usize);
        } else if let Some(value) = arg.strip_prefix("--exclude=") {
            if value.is_empty() {
                bail!("`{arg}` requires a non-empty package name");
            }
            excluded.push(value.to_owned());
            idx = idx.saturating_add(1_usize);
        } else {
            remaining.push(arg.clone());
            idx = idx.saturating_add(1_usize);
        }
    }
    Ok((excluded, remaining))
}

/// Drop `--nocapture` and `--show-output` from `args`.
///
/// Both are libtest stdout/stderr capture toggles. rudzio's structured
/// event output already surfaces test stdout/stderr to the contributor,
/// so there's no rudzio-side knob to flip — but cargo-test users (and
/// AI agents that have memorised cargo-test's flag set) reach for them
/// reflexively, so we accept-and-discard rather than letting them fall
/// through to the runner where they'd warn about an unknown flag.
///
/// Silent consumer (no warning) by design: emitting a "we ignored
/// your flag" message every time would be noise, since the user's
/// intent (see test stdout/stderr) is already satisfied.
#[inline]
#[must_use]
pub fn parse_capture_flags(args: &[String]) -> Vec<String> {
    let mut remaining = Vec::with_capacity(args.len());
    for arg in args {
        if arg != "--nocapture" && arg != "--show-output" {
            remaining.push(arg.clone());
        }
    }
    remaining
}

/// Drop `--no-run` from `args` and return whether it was present.
///
/// Mirrors cargo test's `--no-run`: a unit flag (no value), repeatable
/// without semantic effect — present means "build the aggregator
/// binary but don't execute it". Consumed locally because we have to
/// branch from `cargo run` to `cargo build` ourselves; cargo can't see
/// it through the `cargo run` we'd otherwise spawn.
#[inline]
#[must_use]
pub fn parse_no_run_flag(args: &[String]) -> (bool, Vec<String>) {
    let mut found = false;
    let mut remaining = Vec::with_capacity(args.len());
    for arg in args {
        if arg == "--no-run" {
            found = true;
        } else {
            remaining.push(arg.clone());
        }
    }
    (found, remaining)
}

/// Pull `-p <name>` / `-p=<name>` / `--package <name>` / `--package=<name>` out of `args`.
///
/// Returns the collected package names and the args list with those
/// entries removed (so downstream parsing — path restriction, runner
/// forwarding — only ever sees what's left).
///
/// Mirrors cargo's own `-p` semantics: repeatable, takes one name per
/// occurrence, name match is exact against the Cargo package name
/// (hyphenated form). Without this consumption, `-p` would land in
/// the aggregator's argv where the rudzio runner would warn about an
/// unrecognised flag and treat the package name as a positional
/// substring filter that almost never matches a fully-qualified test.
///
/// # Errors
///
/// Returns an error when `-p` / `--package` is the last arg with no
/// following value, or when the equals form supplies an empty name.
#[inline]
pub fn parse_package_filters(args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut packages = Vec::new();
    let mut remaining = Vec::new();
    let mut idx = 0_usize;
    while let Some(arg) = args.get(idx) {
        if arg == "-p" || arg == "--package" {
            let next_idx = idx.saturating_add(1_usize);
            let value = args
                .get(next_idx)
                .ok_or_else(|| anyhow::anyhow!("`{arg}` requires a package name"))?;
            packages.push(value.clone());
            idx = next_idx.saturating_add(1_usize);
        } else if let Some(value) = arg
            .strip_prefix("-p=")
            .or_else(|| arg.strip_prefix("--package="))
        {
            if value.is_empty() {
                bail!("`{arg}` requires a non-empty package name");
            }
            packages.push(value.to_owned());
            idx = idx.saturating_add(1_usize);
        } else {
            remaining.push(arg.clone());
            idx = idx.saturating_add(1_usize);
        }
    }
    Ok((packages, remaining))
}

/// Pull cargo's per-target selection flags out of `args`.
///
/// Recognised: `--lib`, `--bins`, `--examples`, `--tests`, `--benches`,
/// `--all-targets`, `--doc` (unit flags) and `--bin <NAME>`, `--bin=<NAME>`,
/// `--example <NAME>`, `--example=<NAME>`, `--test <NAME>`, `--test=<NAME>`,
/// `--bench <NAME>`, `--bench=<NAME>` (value flags).
///
/// All are consumed but not honoured — the rudzio aggregator is one
/// binary, so per-target selection has no semantic analog. The
/// returned `Vec<String>` records what was consumed, in input order,
/// so the caller can emit a single consolidated warning rather than a
/// silent no-op (silence here would be confusing: the user thought
/// they were narrowing the run, and got a workspace-wide one
/// instead).
///
/// # Errors
///
/// Returns an error when `--bin` / `--example` / `--test` / `--bench`
/// is the last arg with no following value, or when the equals form
/// supplies an empty name.
#[inline]
pub fn parse_target_selection_flags(args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut consumed = Vec::new();
    let mut remaining = Vec::new();
    let mut idx = 0_usize;
    while let Some(arg) = args.get(idx) {
        if matches!(
            arg.as_str(),
            "--lib" | "--bins" | "--examples" | "--tests" | "--benches" | "--all-targets" | "--doc"
        ) {
            consumed.push(arg.clone());
            idx = idx.saturating_add(1_usize);
        } else if matches!(arg.as_str(), "--bin" | "--example" | "--test" | "--bench") {
            let next_idx = idx.saturating_add(1_usize);
            let value = args
                .get(next_idx)
                .ok_or_else(|| anyhow::anyhow!("`{arg}` requires a target name"))?;
            consumed.push(format!("{arg} {value}"));
            idx = next_idx.saturating_add(1_usize);
        } else if let Some(value_pos) = arg.find('=') {
            let head = arg.get(..value_pos).unwrap_or("");
            if matches!(head, "--bin" | "--example" | "--test" | "--bench") {
                let value = arg.get(value_pos.saturating_add(1_usize)..).unwrap_or("");
                if value.is_empty() {
                    bail!("`{head}=` requires a non-empty target name");
                }
                consumed.push(arg.clone());
            } else {
                remaining.push(arg.clone());
            }
            idx = idx.saturating_add(1_usize);
        } else {
            remaining.push(arg.clone());
            idx = idx.saturating_add(1_usize);
        }
    }
    Ok((consumed, remaining))
}

/// Render the consolidated stderr warning for ignored target flags.
///
/// Returns `None` when the input is empty (no warning to emit), so the
/// caller's print site can be a flat `if let Some(msg) =` without
/// needing its own emptiness check. Returning a single multi-line
/// `String` rather than streaming directly keeps the formatter pure
/// and unit-testable.
#[inline]
#[must_use]
pub fn format_target_flag_warning(consumed: &[String]) -> Option<String> {
    if consumed.is_empty() {
        return None;
    }
    let joined = consumed.join(", ");
    Some(format!(
        "ignored cargo target-selection flags: {joined} (the rudzio aggregator is one binary; \
         per-target selection has no analog — every workspace member's tests will run)"
    ))
}

/// Compose every `cargo rudzio test` parser into a single structured result.
///
/// `is_dir` is injected so the path-vs-runner split is testable without
/// touching disk. Production callers pass `Path::is_dir`; tests pass a
/// closure that recognises a curated set of paths.
///
/// Parser order is: `-p`/`--package` → `--exclude` → `--no-run` →
/// `--workspace`/`--all` → `--nocapture`/`--show-output` →
/// target-selection (`--lib`, `--bin <NAME>`, etc.) → positional paths,
/// with everything else flowing into `runner_args` in original order.
/// Each step operates on what the previous step left behind, so a flag
/// consumed early can never collide with a later one.
///
/// # Errors
///
/// Bubbles errors from any underlying parser (missing values, empty
/// equals-form values).
#[inline]
pub fn parse_test_args<F>(args: &[String], is_dir: F) -> Result<ParsedTestArgs>
where
    F: Fn(&Path) -> bool,
{
    let (include_packages, after_packages) = parse_package_filters(args)?;
    let (exclude_packages, after_excludes) = parse_exclude_filters(&after_packages)?;
    let (no_run, after_no_run) = parse_no_run_flag(&after_excludes);
    let after_workspace = parse_workspace_flag(&after_no_run);
    let after_capture = parse_capture_flags(&after_workspace);
    let (ignored_target_flags, after_targets) = parse_target_selection_flags(&after_capture)?;
    let (include_paths, runner_args) = split_path_args(&after_targets, is_dir);
    Ok(ParsedTestArgs {
        filters: PlanFilters {
            exclude_packages,
            include_packages,
            include_paths,
        },
        ignored_target_flags,
        no_run,
        runner_args,
    })
}

/// Drop `--workspace` and its `--all` alias from `args`.
///
/// `cargo rudzio test` already operates on every workspace member by
/// default (the aggregator is built from a workspace-wide `Plan`), so
/// these flags are redundant — but they're the cargo-test invocations
/// most contributors and AI agents reach for first ("run all the
/// tests"). Accepting and discarding them keeps the muscle-memory path
/// from emitting an "unrecognised flag" warning at the runner.
///
/// Silent consumer (no warning): the user got exactly what they asked
/// for, since the flag matches the default behaviour.
#[inline]
#[must_use]
pub fn parse_workspace_flag(args: &[String]) -> Vec<String> {
    let mut remaining = Vec::with_capacity(args.len());
    for arg in args {
        if arg != "--workspace" && arg != "--all" {
            remaining.push(arg.clone());
        }
    }
    remaining
}

/// Split positional `cargo rudzio test` args into directory paths
/// (used to restrict the aggregator to a subset of workspace members)
/// and runner-bound args (filters, --skip, etc., forwarded verbatim).
///
/// An arg counts as a path iff it is path-shaped (starts with `./`,
/// `../`, `/`, or is exactly `.` / `..`) AND `is_dir` returns `true`
/// for it. The directory check guards against runner filters that
/// happen to look path-shaped — rudzio test names use `::`, so a real
/// path arg practically must exist as a directory at the time of the
/// run. `is_dir` is injected so tests can drive synthetic paths.
#[inline]
pub fn split_path_args<F>(args: &[String], is_dir: F) -> (Vec<PathBuf>, Vec<String>)
where
    F: Fn(&Path) -> bool,
{
    let mut paths = Vec::new();
    let mut runner = Vec::new();
    for arg in args {
        if is_existing_dir_path_arg(arg, &is_dir) {
            paths.push(PathBuf::from(arg));
        } else {
            runner.push(arg.clone());
        }
    }
    (paths, runner)
}

/// Return `true` iff `arg` looks path-shaped AND `is_dir` accepts it.
#[inline]
fn is_existing_dir_path_arg<F>(arg: &str, is_dir: &F) -> bool
where
    F: Fn(&Path) -> bool,
{
    let path_shaped = arg == "."
        || arg == ".."
        || arg.starts_with("./")
        || arg.starts_with("../")
        || arg.starts_with('/');
    path_shaped && is_dir(Path::new(arg))
}
