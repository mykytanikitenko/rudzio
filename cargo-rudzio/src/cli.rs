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
            runner_args: Vec::new(),
        }
    }
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

/// Compose every `cargo rudzio test` parser into a single structured result.
///
/// `is_dir` is injected so the path-vs-runner split is testable without
/// touching disk. Production callers pass `|p| p.is_dir()`; tests pass
/// a closure that recognises a curated set of paths.
///
/// Parser order is: `-p`/`--package` → `--exclude` → positional paths,
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
    let (include_paths, runner_args) = split_path_args(&after_excludes, is_dir);
    Ok(ParsedTestArgs {
        filters: PlanFilters {
            exclude_packages,
            include_packages,
            include_paths,
        },
        runner_args,
    })
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
