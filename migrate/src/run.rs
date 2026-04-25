//! Top-level orchestration. Lives here so `src/main.rs` can stay a
//! trivial binary and the lib-based callers (integration tests, a
//! hypothetical library user) can drive the same flow.

use std::io::{self, BufRead as _, Write as _};
use std::path::Path;
use std::process::ExitCode;

use crate::{cli, discovery, emit, manifest, preflight, report, runner_scaffold, test_context};

pub fn entry() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let parsed = match cli::parse(args) {
        Ok(c) => c,
        Err(cli::ParseError::HelpRequested) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("rudzio-migrate: {err}");
            eprintln!();
            eprint!("{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };

    match run(&parsed) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rudzio-migrate: {err:?}");
            ExitCode::from(2)
        }
    }
}

pub fn run(args: &cli::Cli) -> anyhow::Result<ExitCode> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdin_locked = stdin.lock();
    let mut stdout_locked = stdout.lock();

    let repo_root = match preflight::git_root(&args.path) {
        Ok(p) => p,
        Err(preflight::PreflightError::NotAGitRepo(p)) => {
            eprintln!(
                "rudzio-migrate: not inside a git repository (checked from {}).",
                p.display()
            );
            eprintln!(
                "This tool requires a git repo with a clean working tree so that `git diff` is a reliable review surface."
            );
            return Ok(ExitCode::from(1));
        }
        Err(err) => anyhow::bail!("preflight error: {err}"),
    };

    match preflight::require_clean_tree(&repo_root) {
        Ok(()) => {}
        Err(preflight::PreflightError::DirtyTree) => {
            print!("{}", preflight::DIRTY_TREE_MESSAGE);
            return Ok(ExitCode::from(1));
        }
        Err(err) => anyhow::bail!("preflight error: {err}"),
    }

    write!(stdout_locked, "{}", preflight::INTRO_MESSAGE)?;
    stdout_locked.flush()?;
    match preflight::require_acknowledgement(&mut stdin_locked, &mut stdout_locked) {
        Ok(()) => {}
        Err(preflight::PreflightError::WrongAcknowledgement) => {
            writeln!(stdout_locked, "aborted: acknowledgement did not match.")?;
            return Ok(ExitCode::from(1));
        }
        Err(err) => anyhow::bail!("preflight error: {err}"),
    }

    let want_shared_runner = if args.no_shared_runner {
        false
    } else {
        writeln!(
            stdout_locked,
            "\nGenerate a shared #[rudzio::main] entry and wire Cargo.toml so all tests\nrun through one binary? This modifies Cargo.toml and creates or appends\ntests/main.rs. [y/N]"
        )?;
        stdout_locked.flush()?;
        let mut reply = String::new();
        let n = stdin_locked.read_line(&mut reply)?;
        if n == 0 {
            false
        } else {
            matches!(
                reply.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            )
        }
    };

    // Discovery uses the user-supplied --path (not the git root) so a
    // multi-repo / multi-workspace tree like platform-backend works:
    // the clean-tree check enforces the whole git repo is tidy, but
    // cargo metadata runs at the specific package / workspace the user
    // is migrating, which may be a subdirectory.
    let mut packages = discovery::discover(&args.path)?;
    if let Some(filter) = args.only_package.as_deref() {
        let before = packages.len();
        packages.retain(|p| p.name == filter);
        if packages.is_empty() {
            anyhow::bail!(
                "--only-package {filter:?} did not match any workspace member (checked {before} packages)"
            );
        }
    }
    let test_contexts = test_context::resolve(&packages)?;

    let mut report = report::Report::new();
    let emit_opts = emit::EmitOptions {
        default_runtime: args.runtime,
        preserve_originals: !args.no_preserve_originals,
        dry_run: args.dry_run,
        test_contexts: &test_contexts,
    };

    for pkg in &packages {
        let mut pkg_edits = manifest::ManifestEdits::default();
        let mut pkg_had_conversions = false;
        for file in pkg.src_files.iter().chain(pkg.tests_files.iter()) {
            match emit::process_file(file, &emit_opts, &mut report) {
                Ok(Some(rewrite)) => {
                    pkg_had_conversions = true;
                    pkg_edits.runtimes.extend(rewrite.runtimes_used.iter().copied());
                    pkg_edits.needs_anyhow |= rewrite.needs_anyhow;
                    if is_under_tests_dir(file, &pkg.root) {
                        if let Some(name) = file.file_stem().and_then(|s| s.to_str()) {
                            pkg_edits.tests_integration.push(manifest::IntegrationTestEntry {
                                name: name.to_owned(),
                                path: format!("tests/{name}.rs"),
                            });
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    report.warn(
                        file.clone(),
                        None,
                        format!("error processing file: {err:#}"),
                    );
                }
            }
        }
        if want_shared_runner && !args.dry_run && pkg_had_conversions {
            let tests_main = pkg.root.join("tests").join("main.rs");
            // Lib crate name uses hyphen→underscore per rustc's
            // "hyphens in crate names are normalized" rule.
            let crate_lib_name = pkg.name.replace('-', "_");
            match runner_scaffold::ensure_tests_main(&tests_main, Some(&crate_lib_name)) {
                Ok(runner_scaffold::ScaffoldOutcome::Created) => {
                    report.touched(tests_main);
                    pkg_edits
                        .tests_integration
                        .push(manifest::IntegrationTestEntry {
                            name: "main".to_owned(),
                            path: "tests/main.rs".to_owned(),
                        });
                }
                Ok(runner_scaffold::ScaffoldOutcome::AlreadyExists) => {
                    report.warn(
                        tests_main,
                        None,
                        "tests/main.rs already exists; leaving it alone — add `#[rudzio::main] fn main() {}` yourself if needed",
                    );
                }
                Err(err) => {
                    report.warn(
                        tests_main,
                        None,
                        format!("failed to scaffold tests/main.rs: {err:#}"),
                    );
                }
            }
        }
        if !args.dry_run && pkg_had_conversions {
            match manifest::apply(&pkg.manifest_path, &pkg_edits) {
                Ok(true) => report.cargo_edit(pkg.manifest_path.clone()),
                Ok(false) => {}
                Err(err) => {
                    report.warn(
                        pkg.manifest_path.clone(),
                        None,
                        format!("Cargo.toml edit failed: {err:#}"),
                    );
                }
            }
        }
    }

    report.print_summary(&mut stdout_locked)?;
    Ok(ExitCode::SUCCESS)
}

fn is_under_tests_dir(file: &Path, pkg_root: &Path) -> bool {
    file.starts_with(pkg_root.join("tests"))
}
