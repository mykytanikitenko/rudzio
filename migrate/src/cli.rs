//! Hand-rolled argv parser. Kept small on purpose: no clap dependency
//! for a binary whose surface is five flags.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Cli {
    pub path: PathBuf,
    pub runtime: RuntimeChoice,
    pub dry_run: bool,
    pub no_shared_runner: bool,
    pub no_preserve_originals: bool,
    /// When `Some`, restrict migration to the named workspace member
    /// (matched against `cargo_metadata`'s `Package::name`). Useful
    /// for incremental rollouts across large workspaces.
    pub only_package: Option<String>,
    /// Skip `src/**/*.rs` during the conversion pass — only files
    /// under `tests/` are migrated. Useful for crates whose `src/`
    /// is dense with macro invocations (e.g. `ambassador`,
    /// delegation crates, procedural wrappers) that syn parses but
    /// prettyplease can't round-trip. The lib keeps its existing
    /// `#[cfg(test)] mod tests { ... }` harness unchanged.
    pub tests_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeChoice {
    TokioMt,
    TokioCt,
    Compio,
    FuturesMt,
    FuturesCt,
}

impl RuntimeChoice {
    pub const fn suite_path(self) -> &'static str {
        match self {
            Self::TokioMt => "::rudzio::runtime::tokio::Multithread::new",
            Self::TokioCt => "::rudzio::runtime::tokio::CurrentThread::new",
            Self::Compio => "::rudzio::runtime::compio::Compio::new",
            Self::FuturesMt => "::rudzio::runtime::futures::Multithread::new",
            Self::FuturesCt => "::rudzio::runtime::futures::CurrentThread::new",
        }
    }

    pub const fn cargo_feature(self) -> &'static str {
        match self {
            Self::TokioMt | Self::TokioCt => "runtime-tokio",
            Self::Compio => "runtime-compio",
            Self::FuturesMt | Self::FuturesCt => "runtime-futures",
        }
    }
}

#[derive(Debug)]
pub enum ParseError {
    UnknownFlag(String),
    MissingValue(String),
    UnknownRuntime(String),
    HelpRequested,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownFlag(s) => write!(f, "unknown flag: {s}"),
            Self::MissingValue(s) => write!(f, "missing value for {s}"),
            Self::UnknownRuntime(s) => write!(
                f,
                "unknown runtime `{s}` — pick one of: tokio-mt, tokio-ct, compio, futures-mt, futures-ct"
            ),
            Self::HelpRequested => write!(f, "help"),
        }
    }
}

impl std::error::Error for ParseError {}

pub const USAGE: &str = "\
rudzio-migrate — best-effort converter of Rust tests into rudzio tests.

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
    --tests-only            Skip src/**/*.rs during conversion — only
                            tests/ files are migrated. Use when src/
                            is dense with macros (`ambassador`,
                            delegation) that syn parses but
                            prettyplease can't round-trip.
    --help, -h              Print this message.

NOTE: The tool refuses to run on a dirty git tree and requires the user
to type an acknowledgement phrase. Both gates are load-bearing safety
features — there is no bypass flag.
";

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, ParseError> {
    let mut path: Option<PathBuf> = None;
    let mut runtime = RuntimeChoice::TokioMt;
    let mut dry_run = false;
    let mut no_shared_runner = false;
    let mut no_preserve_originals = false;
    let mut only_package: Option<String> = None;
    let mut tests_only = false;

    let mut it = args.into_iter();
    let _program = it.next();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(ParseError::HelpRequested),
            "--dry-run" => dry_run = true,
            "--no-shared-runner" => no_shared_runner = true,
            "--no-preserve-originals" => no_preserve_originals = true,
            "--tests-only" => tests_only = true,
            "--path" => {
                let value = it
                    .next()
                    .ok_or_else(|| ParseError::MissingValue("--path".to_owned()))?;
                path = Some(PathBuf::from(value));
            }
            "--runtime" => {
                let value = it
                    .next()
                    .ok_or_else(|| ParseError::MissingValue("--runtime".to_owned()))?;
                runtime = parse_runtime(&value)?;
            }
            "--only-package" => {
                let value = it
                    .next()
                    .ok_or_else(|| ParseError::MissingValue("--only-package".to_owned()))?;
                only_package = Some(value);
            }
            s if s.starts_with("--path=") => {
                path = Some(PathBuf::from(&s["--path=".len()..]));
            }
            s if s.starts_with("--runtime=") => {
                runtime = parse_runtime(&s["--runtime=".len()..])?;
            }
            s if s.starts_with("--only-package=") => {
                only_package = Some(s["--only-package=".len()..].to_owned());
            }
            other => return Err(ParseError::UnknownFlag(other.to_owned())),
        }
    }

    let path = match path {
        Some(p) => p,
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };

    Ok(Cli {
        path,
        runtime,
        dry_run,
        no_shared_runner,
        no_preserve_originals,
        only_package,
        tests_only,
    })
}

fn parse_runtime(s: &str) -> Result<RuntimeChoice, ParseError> {
    match s {
        "tokio-mt" => Ok(RuntimeChoice::TokioMt),
        "tokio-ct" => Ok(RuntimeChoice::TokioCt),
        "compio" => Ok(RuntimeChoice::Compio),
        "futures-mt" => Ok(RuntimeChoice::FuturesMt),
        "futures-ct" => Ok(RuntimeChoice::FuturesCt),
        other => Err(ParseError::UnknownRuntime(other.to_owned())),
    }
}
