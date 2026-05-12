//! Hand-rolled argv parser. Kept small on purpose: no clap dependency
//! for a binary whose surface is five flags.

use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

/// Help text printed for `--help` / `-h`.
pub const USAGE: &str = "\
rudzio-migrate \u{2014} best-effort converter of Rust tests into rudzio tests.

USAGE:
    rudzio-migrate [OPTIONS]

OPTIONS:
    --path <DIR>            Repo root (default: current working directory;
                            must be inside a git repo).
    --runtime <NAME>        Default runtime for generated suites. One of:
                            tokio-mt (default), tokio-ct, compio,
                            futures-mt, futures-ct. Explicit per-test
                            flavors in #[tokio::test(...)] override this.
    --dry-run               Parse and report planned changes; do not
                            write any files, do not create backups.
    --no-shared-runner      Skip the Cargo.toml + tests/main.rs
                            scaffolding prompt.
    --no-preserve-originals Do not emit a pre-migration block comment
                            above each converted fn.
    --only-package <NAME>   Restrict the run to a single workspace member
                            (matched against the `cargo metadata` package
                            name). Other packages are left alone.
    --tests-only            Skip src/**/*.rs during conversion \u{2014} only
                            tests/ files are migrated. Use when src/
                            is dense with macros (`ambassador`,
                            delegation) that syn parses but
                            prettyplease can't round-trip.
    --help, -h              Print this message.

NOTE: The tool refuses to run on a dirty git tree and requires the user
to type an acknowledgement phrase. Both gates are load-bearing safety
features \u{2014} there is no bypass flag.
";

/// Parsed command-line arguments.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Cli {
    /// Bool flags governing what gets generated. Split from
    /// [`RunFlags`] so neither substruct trips
    /// `struct_excessive_bools`.
    pub generation: GenerationFlags,
    /// When `Some`, restrict migration to the named workspace member
    /// (matched against `cargo_metadata`'s `Package::name`). Useful
    /// for incremental rollouts across large workspaces.
    pub only_package: Option<String>,
    /// Repo root the run targets.
    pub path: PathBuf,
    /// Bool flags governing run behaviour (mode + scope).
    pub run: RunFlags,
    /// Default runtime baked into generated suites when no
    /// per-attribute flavor is explicit.
    pub runtime: RuntimeChoice,
}

/// Bool flags that govern what the generator emits.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct GenerationFlags {
    /// Don't emit a pre-migration block comment above each converted
    /// fn.
    pub no_preserve_originals: bool,
    /// Skip the Cargo.toml + `tests/main.rs` scaffolding prompt.
    pub no_shared_runner: bool,
}

/// Reasons argv parsing may fail.
#[derive(Debug)]
#[non_exhaustive]
pub enum ParseError {
    /// `--help` or `-h` was passed.
    HelpRequested,
    /// A flag that takes a value was passed without one.
    MissingValue(String),
    /// An argument that doesn't match any known flag was passed.
    UnknownFlag(String),
    /// `--runtime <NAME>` got a name we don't recognise.
    UnknownRuntime(String),
}

/// Bool flags that govern the run's mode and input scope.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct RunFlags {
    /// Parse and report planned changes; don't write files or create
    /// backups.
    pub dry_run: bool,
    /// Skip `src/**/*.rs` during the conversion pass — only files
    /// under `tests/` are migrated. Useful for crates whose `src/`
    /// is dense with macro invocations (e.g. `ambassador`,
    /// delegation crates, procedural wrappers) that syn parses but
    /// prettyplease can't round-trip. The lib keeps its existing
    /// `#[cfg(test)] mod tests { ... }` harness unchanged.
    pub tests_only: bool,
}

/// Choice of runtime baked into the generated suite blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RuntimeChoice {
    /// `compio`.
    Compio,
    /// `futures` current-thread.
    FuturesCt,
    /// `futures` multi-thread.
    FuturesMt,
    /// Tokio current-thread (`flavor = "current_thread"`).
    TokioCt,
    /// Tokio multi-thread (the default).
    TokioMt,
}

impl fmt::Display for ParseError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelpRequested => write!(f, "help"),
            Self::MissingValue(flag) => write!(f, "missing value for {flag}"),
            Self::UnknownFlag(flag) => write!(f, "unknown flag: {flag}"),
            Self::UnknownRuntime(name) => write!(
                f,
                "unknown runtime `{name}` — pick one of: tokio-mt, tokio-ct, compio, futures-mt, futures-ct",
            ),
        }
    }
}

impl StdError for ParseError {}

impl RuntimeChoice {
    /// Cargo feature gate this runtime corresponds to.
    #[inline]
    #[must_use]
    pub const fn cargo_feature(self) -> &'static str {
        match self {
            Self::Compio => "runtime-compio",
            Self::FuturesCt | Self::FuturesMt => "runtime-futures",
            Self::TokioCt => "runtime-tokio-current-thread",
            Self::TokioMt => "runtime-tokio-multi-thread",
        }
    }

    /// Fully-qualified path to the runtime's `new` constructor, ready
    /// to drop into a generated `runtime = ...` field.
    #[inline]
    #[must_use]
    pub const fn suite_path(self) -> &'static str {
        match self {
            Self::Compio => "::rudzio::runtime::compio::Compio::new",
            Self::FuturesCt => "::rudzio::runtime::futures::CurrentThread::new",
            Self::FuturesMt => "::rudzio::runtime::futures::Multithread::new",
            Self::TokioCt => "::rudzio::runtime::tokio::CurrentThread::new",
            Self::TokioMt => "::rudzio::runtime::tokio::Multithread::new",
        }
    }
}

/// Parse argv into a [`Cli`].
///
/// # Errors
///
/// Returns [`ParseError::HelpRequested`] for `--help`/`-h`,
/// [`ParseError::UnknownFlag`] for unrecognised arguments,
/// [`ParseError::MissingValue`] when a flag that needs a value got
/// none, and [`ParseError::UnknownRuntime`] when `--runtime <NAME>`
/// gets an unknown name.
#[inline]
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, ParseError> {
    let mut path: Option<PathBuf> = None;
    let mut runtime = RuntimeChoice::TokioMt;
    let mut run = RunFlags::default();
    let mut generation = GenerationFlags::default();
    let mut only_package: Option<String> = None;

    let mut iter = args.into_iter();
    let _program = iter.next();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(ParseError::HelpRequested),
            "--dry-run" => run.dry_run = true,
            "--no-shared-runner" => generation.no_shared_runner = true,
            "--no-preserve-originals" => generation.no_preserve_originals = true,
            "--tests-only" => run.tests_only = true,
            "--path" => {
                let value = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue("--path".to_owned()))?;
                path = Some(PathBuf::from(value));
            }
            "--runtime" => {
                let value = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue("--runtime".to_owned()))?;
                runtime = parse_runtime(&value)?;
            }
            "--only-package" => {
                let value = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue("--only-package".to_owned()))?;
                only_package = Some(value);
            }
            flag if flag.starts_with("--path=") => {
                path = Some(PathBuf::from(flag.get("--path=".len()..).unwrap_or("")));
            }
            flag if flag.starts_with("--runtime=") => {
                runtime = parse_runtime(flag.get("--runtime=".len()..).unwrap_or(""))?;
            }
            flag if flag.starts_with("--only-package=") => {
                only_package = Some(flag.get("--only-package=".len()..).unwrap_or("").to_owned());
            }
            other => return Err(ParseError::UnknownFlag(other.to_owned())),
        }
    }

    let resolved_path =
        path.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_err| PathBuf::from(".")));

    Ok(Cli {
        generation,
        only_package,
        path: resolved_path,
        run,
        runtime,
    })
}

/// Map a `--runtime` value to a [`RuntimeChoice`].
fn parse_runtime(name: &str) -> Result<RuntimeChoice, ParseError> {
    match name {
        "tokio-mt" => Ok(RuntimeChoice::TokioMt),
        "tokio-ct" => Ok(RuntimeChoice::TokioCt),
        "compio" => Ok(RuntimeChoice::Compio),
        "futures-mt" => Ok(RuntimeChoice::FuturesMt),
        "futures-ct" => Ok(RuntimeChoice::FuturesCt),
        other => Err(ParseError::UnknownRuntime(other.to_owned())),
    }
}
