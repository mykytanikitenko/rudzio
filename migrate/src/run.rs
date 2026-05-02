//! Top-level orchestration. Lives here so `src/main.rs` can stay a
//! trivial binary and the lib-based callers (integration tests, a
//! hypothetical library user) can drive the same flow.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead as _, Write as _};
use std::iter;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use rudzio::output::{write_stderr, write_stdout};

use crate::{backup, cli, discovery, emit, manifest, preflight, report, runner_scaffold, test_context};

/// Prompt text for the `--no-shared-runner=false` case.
const SHARED_RUNNER_PROMPT: &str = "\nGenerate a shared #[rudzio::main] entry and wire Cargo.toml so all tests\nrun through one binary? This modifies Cargo.toml and creates or appends\ntests/main.rs. [y/N]\n";

/// Warning text emitted when `tests/main.rs` already exists.
const TESTS_MAIN_EXISTS_WARNING: &str = "tests/main.rs already exists; leaving it and its [[test]] entry alone \u{2014} add `#[rudzio::main] fn main() {}` yourself if the file doesn't already host one";

/// Drive the migrator using the ambient process argv.
#[inline]
#[must_use]
pub fn entry() -> ExitCode {
    entry_with_args(env::args().collect())
}

/// Same as [`entry`] but takes argv explicitly so embedders (e.g.
/// the `cargo-rudzio migrate` subcommand) can drive the migrator
/// without relying on the ambient process argv.
#[inline]
#[must_use]
pub fn entry_with_args(args: Vec<String>) -> ExitCode {
    let parsed = match cli::parse(args) {
        Ok(parsed_cli) => parsed_cli,
        Err(cli::ParseError::HelpRequested) => {
            write_stdout(cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            write_stderr(&format!("rudzio-migrate: {err}\n\n"));
            write_stderr(cli::USAGE);
            return ExitCode::from(2);
        }
    };

    match run(&parsed) {
        Ok(code) => code,
        Err(err) => {
            write_stderr(&format!("rudzio-migrate: {err:#}\n"));
            ExitCode::from(2)
        }
    }
}

/// Drive the full migration pipeline against `args`.
///
/// # Errors
///
/// Returns the underlying error if discovery, the manifest reader,
/// the rewriter, or the report printer fail in ways that aren't
/// converted into per-file warnings on `report`.
#[inline]
pub fn run(args: &cli::Cli) -> anyhow::Result<ExitCode> {
    let stdout = io::stdout();
    let mut stdout_locked = stdout.lock();

    if let Some(exit) = run_preflight(args, &mut stdout_locked)? {
        return Ok(exit);
    }
    let Some(want_shared_runner) = prompt_shared_runner(args, &mut stdout_locked)? else {
        return Ok(ExitCode::from(1));
    };

    let mut packages = discovery::discover(&args.path)?;
    if let Some(filter) = args.only_package.as_deref() {
        let before = packages.len();
        packages.retain(|pkg| pkg.name == filter);
        if packages.is_empty() {
            anyhow::bail!(
                "--only-package {filter:?} did not match any workspace member (checked {before} packages)"
            );
        }
    }
    let test_contexts = test_context::resolve(&packages)?;
    let workspace_dep_names = collect_workspace_dep_names(&args.path);

    let mut report = report::Report::new();
    let emit_opts = emit::EmitOptions {
        default_runtime: args.runtime,
        preserve_originals: !args.generation.no_preserve_originals,
        dry_run: args.run.dry_run,
        test_contexts: &test_contexts,
    };

    for pkg in &packages {
        process_package(
            args,
            pkg,
            want_shared_runner,
            &workspace_dep_names,
            &emit_opts,
            &mut report,
        );
    }

    report.print_summary(&mut stdout_locked)?;
    Ok(ExitCode::SUCCESS)
}

/// Run the three preflight gates (git repo, clean tree, ack phrase)
/// and return `Some(exit)` when one of them aborts the run.
fn run_preflight(
    args: &cli::Cli,
    stdout: &mut io::StdoutLock<'_>,
) -> anyhow::Result<Option<ExitCode>> {
    let repo_root = match preflight::git_root(&args.path) {
        Ok(root) => root,
        Err(preflight::Failure::NotAGitRepo(checked)) => {
            write_stderr(&format!(
                "rudzio-migrate: not inside a git repository (checked from {}).\n",
                checked.display(),
            ));
            write_stderr(
                "This tool requires a git repo with a clean working tree so that `git diff` is a reliable review surface.\n",
            );
            return Ok(Some(ExitCode::from(1)));
        }
        Err(err) => anyhow::bail!("preflight error: {err}"),
    };

    match preflight::require_clean_tree(&repo_root) {
        Ok(()) => {}
        Err(preflight::Failure::DirtyTree) => {
            write_stdout(preflight::DIRTY_TREE_MESSAGE);
            return Ok(Some(ExitCode::from(1)));
        }
        Err(err) => anyhow::bail!("preflight error: {err}"),
    }

    write!(stdout, "{}", preflight::INTRO_MESSAGE)?;
    stdout.flush()?;
    let stdin = io::stdin();
    let mut stdin_locked = stdin.lock();
    match preflight::require_acknowledgement(&mut stdin_locked, &mut *stdout) {
        Ok(()) => Ok(None),
        Err(preflight::Failure::WrongAcknowledgement) => {
            writeln!(stdout, "aborted: acknowledgement did not match.")?;
            Ok(Some(ExitCode::from(1)))
        }
        Err(err) => anyhow::bail!("preflight error: {err}"),
    }
}

/// Prompt for the shared-runner scaffold. Returns `Some(true)` /
/// `Some(false)` for the user's answer, or `None` if reading stdin
/// failed in a way that should abort the run.
fn prompt_shared_runner(
    args: &cli::Cli,
    stdout: &mut io::StdoutLock<'_>,
) -> anyhow::Result<Option<bool>> {
    if args.generation.no_shared_runner {
        return Ok(Some(false));
    }
    write!(stdout, "{SHARED_RUNNER_PROMPT}")?;
    stdout.flush()?;
    let mut reply = String::new();
    let bytes_read = {
        let stdin = io::stdin();
        let mut stdin_locked = stdin.lock();
        stdin_locked.read_line(&mut reply)?
    };
    if bytes_read == 0 {
        return Ok(Some(false));
    }
    Ok(Some(matches!(
        reply.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    )))
}

/// Process a single package: rewrite its files, emit auxiliary edits
/// (`forbid(unsafe)` demotion, `fn main` injections, scaffold), then
/// apply the Cargo.toml edits.
fn process_package(
    args: &cli::Cli,
    pkg: &discovery::Package,
    want_shared_runner: bool,
    workspace_dep_names: &BTreeSet<String>,
    emit_opts: &emit::EmitOptions<'_>,
    report: &mut report::Report,
) {
    let mut pkg_edits = manifest::Edits {
        workspace_dep_names: workspace_dep_names.clone(),
        has_lib_rs: pkg.root.join("src/lib.rs").is_file(),
        bin_names: pkg.bin_names.clone(),
        ..manifest::Edits::default()
    };
    // Suite roots (`tests/<suite>/mod.rs`) whose subtree had files
    // modified. The rewriter only appends `fn main` to a binary-root
    // file that itself changed; for subdir layouts the mod.rs is
    // wiring-only and stays unchanged, but the binary still needs a
    // `fn main` to link. Fixed up below.
    let mut suite_roots_needing_main: BTreeSet<PathBuf> = BTreeSet::new();
    let pkg_had_conversions = process_package_files(
        args,
        pkg,
        emit_opts,
        &mut pkg_edits,
        &mut suite_roots_needing_main,
        report,
    );
    // `#[rudzio::suite]` expansion emits `#[allow(unsafe_code)]`
    // (linkme registers via `#[link_section]`, which rustc
    // classifies as unsafe_code). On crates whose `src/lib.rs`
    // carries `#![forbid(unsafe_code)]` the inner allow conflicts
    // — `forbid` doesn't accept downstream `allow` overrides.
    // Demote `forbid` to `deny` when this package had any src
    // conversion, so the user's intent (no unsafe in their code)
    // is preserved while leaving room for the macro expansion.
    if pkg_edits.had_src_conversion && !args.run.dry_run {
        finalize_lib_forbid_demotion(pkg, report);
        finalize_lib_main(pkg, report);
    }
    if !args.run.dry_run {
        finalize_suite_main_injections(&suite_roots_needing_main, report);
    }
    if want_shared_runner && !args.run.dry_run && pkg_had_conversions {
        finalize_shared_runner_scaffold(pkg, &mut pkg_edits, report);
    }
    if pkg_had_conversions {
        pkg_edits.needs.rudzio_test_cfg = true;
    }
    if !args.run.dry_run && pkg_had_conversions {
        finalize_manifest_apply(pkg, &pkg_edits, report);
    }
}

/// Iterate over every `src/` and `tests/` file in `pkg`, run the
/// rewriter on each, and accumulate the per-file edits into
/// `pkg_edits` and the file-system follow-ups into
/// `suite_roots_needing_main`. Returns whether any file in the
/// package was rewritten.
fn process_package_files(
    args: &cli::Cli,
    pkg: &discovery::Package,
    emit_opts: &emit::EmitOptions<'_>,
    pkg_edits: &mut manifest::Edits,
    suite_roots_needing_main: &mut BTreeSet<PathBuf>,
    report: &mut report::Report,
) -> bool {
    let mut pkg_had_conversions = false;
    let src_iter: Box<dyn Iterator<Item = &PathBuf>> = if args.run.tests_only {
        Box::new(iter::empty())
    } else {
        Box::new(pkg.src_files.iter())
    };
    for file in src_iter.chain(pkg.tests_files.iter()) {
        match emit::process_file(file, emit_opts, report) {
            Ok(Some(rewrite)) => {
                pkg_had_conversions = true;
                pkg_edits
                    .runtimes
                    .extend(rewrite.runtimes_used.iter().copied());
                pkg_edits.needs.anyhow |= rewrite.needs_anyhow;
                // `runtimes_used` is non-empty iff the rewriter
                // actually promoted a `#[cfg(test)]` module into a
                // `#[rudzio::suite(...)]`. A file whose only
                // rewrite was a cfg_attr broadening
                // (`cfg_attr(test, ...) → cfg_attr(any(test,
                // rudzio_test), ...)`) contributes nothing here,
                // so we skip `had_src_conversion` and the
                // downstream `[lib] harness = false` /
                // `#[rudzio::main]` edits that only make sense
                // when the crate hosts at least one rudzio suite.
                let promoted_suite = !rewrite.runtimes_used.is_empty();
                if promoted_suite && file.starts_with(pkg.root.join("src")) {
                    pkg_edits.had_src_conversion = true;
                }
                if let Some(entry) = integration_test_entry_for(file, &pkg.root) {
                    pkg_edits.tests_integration.push(entry);
                }
                if let Some(root) = suite_root_mod_rs_for(file, &pkg.root) {
                    let _inserted = suite_roots_needing_main.insert(root);
                }
            }
            Ok(None) => {}
            Err(err) => {
                report.warn(file.clone(), None, format!("error processing file: {err:#}"));
            }
        }
    }
    pkg_had_conversions
}

/// Run the `#![forbid(unsafe_code)] → #![deny(unsafe_code)]`
/// downgrade on the package's lib.rs, recording the touched path or
/// a warning on `report`.
fn finalize_lib_forbid_demotion(pkg: &discovery::Package, report: &mut report::Report) {
    match demote_forbid_unsafe_in_lib(&pkg.root) {
        Ok(Some(path)) => report.touched(path),
        Ok(None) => {}
        Err(err) => {
            report.warn(
                pkg.root.join("src/lib.rs"),
                None,
                format!("failed to demote forbid(unsafe_code): {err:#}"),
            );
        }
    }
}

/// Append `#[cfg(test)] #[rudzio::main] fn main() {}` to lib.rs (when
/// applicable), recording the touched path or a warning on `report`.
fn finalize_lib_main(pkg: &discovery::Package, report: &mut report::Report) {
    match ensure_lib_has_rudzio_main(&pkg.root) {
        Ok(Some(path)) => report.touched(path),
        Ok(None) => {}
        Err(err) => {
            report.warn(
                pkg.root.join("src/lib.rs"),
                None,
                format!("failed to append rudzio::main to src/lib.rs: {err:#}"),
            );
        }
    }
}

/// For each suite root `mod.rs` in `roots`, append `#[rudzio::main]
/// fn main() {}` if needed. Records touched paths / warnings on
/// `report`.
fn finalize_suite_main_injections(roots: &BTreeSet<PathBuf>, report: &mut report::Report) {
    for mod_rs in roots {
        match ensure_suite_root_has_main(mod_rs) {
            Ok(true) => report.touched(mod_rs.clone()),
            Ok(false) => {}
            Err(err) => {
                report.warn(
                    mod_rs.clone(),
                    None,
                    format!("failed to ensure fn main in {}: {err:#}", mod_rs.display()),
                );
            }
        }
    }
}

/// Generate the shared `tests/main.rs` runner scaffold and record
/// the synthesised `[[test]] main` entry on `pkg_edits`.
fn finalize_shared_runner_scaffold(
    pkg: &discovery::Package,
    pkg_edits: &mut manifest::Edits,
    report: &mut report::Report,
) {
    let tests_main = pkg.root.join("tests").join("main.rs");
    // Lib crate name uses hyphen→underscore per rustc's "hyphens in
    // crate names are normalized" rule.
    let crate_lib_name = pkg.name.replace('-', "_");
    match runner_scaffold::ensure_tests_main(
        &tests_main,
        Some(&crate_lib_name),
        &pkg.lib_modules,
        pkg.uses_lib_aggregation,
    ) {
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
            // Leave the file alone AND skip the synthesized
            // `[[test]] main` Cargo entry — adding one on top of a
            // manifest that may already describe the same target
            // breaks the manifest. If the user already has rudzio
            // wired up here they don't need another [[test]] block;
            // if not, the warning points them at the right next step.
            report.warn(tests_main, None, TESTS_MAIN_EXISTS_WARNING);
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

/// Apply `pkg_edits` to the package's Cargo.toml and record the
/// outcome on `report`.
fn finalize_manifest_apply(
    pkg: &discovery::Package,
    pkg_edits: &manifest::Edits,
    report: &mut report::Report,
) {
    match manifest::apply(&pkg.manifest_path, pkg_edits) {
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

/// Walk upward from `start` looking for a Cargo.toml whose
/// `[workspace.dependencies]` table is non-empty, and return the
/// set of dep names declared there. When found we use
/// `{ workspace = true, ... }` for dep edits instead of pinning a
/// fresh version per crate.
fn collect_workspace_dep_names(start: &Path) -> BTreeSet<String> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file()
            && let Ok(source) = fs::read_to_string(&candidate)
            && let Ok(doc) = source.parse::<toml_edit::DocumentMut>()
            && let Some(ws_deps) = doc
                .as_table()
                .get("workspace")
                .and_then(toml_edit::Item::as_table)
                .and_then(|table| table.get("dependencies"))
                .and_then(toml_edit::Item::as_table)
        {
            return ws_deps.iter().map(|(key, _)| key.to_owned()).collect();
        }
        if !current.pop() {
            return BTreeSet::new();
        }
    }
}

/// Rewrite `#![forbid(unsafe_code)]` → `#![deny(unsafe_code)]` in
/// the package's `src/lib.rs` when the lib hosts migrated suite
/// blocks. Returns `Ok(Some(path))` when the file was rewritten,
/// `Ok(None)` if there was nothing to do. String-level replace —
/// fast, safe, and the common shape is exact-match.
fn demote_forbid_unsafe_in_lib(pkg_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let lib_rs = pkg_root.join("src/lib.rs");
    if !lib_rs.is_file() {
        return Ok(None);
    }
    let source =
        fs::read_to_string(&lib_rs).with_context(|| format!("reading {}", lib_rs.display()))?;
    let target = "#![forbid(unsafe_code)]";
    if !source.contains(target) {
        return Ok(None);
    }
    let new_source = source.replace(target, "#![deny(unsafe_code)]");
    let _bak = backup::copy_before_write(&lib_rs)
        .with_context(|| format!("backing up {}", lib_rs.display()))?;
    fs::write(&lib_rs, &new_source).with_context(|| format!("writing {}", lib_rs.display()))?;
    Ok(Some(lib_rs))
}

/// Append `#[cfg(test)] #[rudzio::main] fn main() {}` to `src/lib.rs`
/// so the lib's test target (now `harness = false`) has an entry
/// point. Returns `Ok(Some(path))` if appended, `Ok(None)` if lib.rs
/// doesn't exist or already declares a `main`. The cfg(test) gate is
/// critical — without it, binaries that depend on this lib would
/// collide at link time with their own `fn main`.
fn ensure_lib_has_rudzio_main(pkg_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let lib_rs = pkg_root.join("src/lib.rs");
    if !lib_rs.is_file() {
        return Ok(None);
    }
    let source =
        fs::read_to_string(&lib_rs).with_context(|| format!("reading {}", lib_rs.display()))?;
    // Idempotency: if the file already has a `fn main`, don't
    // append another one. The check uses syn (rather than a text
    // `contains`) because doc comments legitimately mention
    // `#[rudzio::main]` in explanatory prose without making the
    // file rudzio-ready. Parse failure → fall through to the
    // append path; worst case the user gets a duplicate `fn main`
    // and a compile error that points straight at the fix.
    if let Ok(tree) = syn::parse_file(&source) {
        let has_main = tree
            .items
            .iter()
            .any(|item| matches!(item, syn::Item::Fn(fn_item) if fn_item.sig.ident == "main"));
        if has_main {
            return Ok(None);
        }
    }
    let _bak = backup::copy_before_write(&lib_rs)
        .with_context(|| format!("backing up {}", lib_rs.display()))?;
    let appended = if source.ends_with('\n') {
        format!("{source}#[cfg(test)]\n#[::rudzio::main]\nfn main() {{}}\n")
    } else {
        format!("{source}\n#[cfg(test)]\n#[::rudzio::main]\nfn main() {{}}\n")
    };
    fs::write(&lib_rs, &appended).with_context(|| format!("writing {}", lib_rs.display()))?;
    Ok(Some(lib_rs))
}

/// Append `#[rudzio::main] fn main() {}` to `mod_rs` if the file
/// exists and doesn't already contain a `fn main`. Returns `Ok(true)`
/// if the file was rewritten, `Ok(false)` if a `fn main` was already
/// there (or the file didn't exist). Makes a backup before writing.
fn ensure_suite_root_has_main(mod_rs: &Path) -> anyhow::Result<bool> {
    if !mod_rs.is_file() {
        return Ok(false);
    }
    let source =
        fs::read_to_string(mod_rs).with_context(|| format!("reading {}", mod_rs.display()))?;
    let Ok(tree) = syn::parse_file(&source) else {
        return Ok(false);
    };
    let has_main = tree
        .items
        .iter()
        .any(|item| matches!(item, syn::Item::Fn(fn_item) if fn_item.sig.ident == "main"));
    if has_main {
        return Ok(false);
    }
    let _bak = backup::copy_before_write(mod_rs)
        .with_context(|| format!("backing up {}", mod_rs.display()))?;
    let appended = if source.ends_with('\n') {
        format!("{source}\n#[rudzio::main]\nfn main() {{}}\n")
    } else {
        format!("{source}\n\n#[rudzio::main]\nfn main() {{}}\n")
    };
    fs::write(mod_rs, appended).with_context(|| format!("writing {}", mod_rs.display()))?;
    Ok(true)
}

/// When `file` is a deeper descendant of `tests/<suite>/` (anything
/// below the suite's `mod.rs`), return the path to that
/// `tests/<suite>/mod.rs` root — otherwise `None`. Used to detect
/// the binary root that needs a `fn main` even though it wasn't
/// directly modified.
fn suite_root_mod_rs_for(file: &Path, pkg_root: &Path) -> Option<PathBuf> {
    let tests_dir = pkg_root.join("tests");
    let rel = file.strip_prefix(&tests_dir).ok()?;
    let mut components = rel.components();
    let suite = components.next()?.as_os_str().to_str()?.to_owned();
    let rest: Vec<&OsStr> = components.map(Component::as_os_str).collect();
    // File IS the suite root; nothing to do here — the rewriter
    // handles fn main injection for files that did change.
    match rest.as_slice() {
        [] => return None,
        [only] if *only == OsStr::new("mod.rs") => return None,
        _ => {}
    }
    Some(tests_dir.join(&suite).join("mod.rs"))
}

/// Returns the `[[test]]` entry for a file if it's a test-binary
/// root — i.e. `tests/<stem>.rs` (direct child) or `tests/<suite>/mod.rs`
/// (mod-file pattern). Files deeper in `tests/` are submodules of a
/// test binary whose root lives elsewhere; synthesising a
/// `[[test]] path = "tests/<stem>.rs"` entry for them would point at
/// a file that doesn't exist and break the build.
fn integration_test_entry_for(
    file: &Path,
    pkg_root: &Path,
) -> Option<manifest::IntegrationTestEntry> {
    let tests_dir = pkg_root.join("tests");
    let rel = file.strip_prefix(&tests_dir).ok()?;
    let mut components = rel.components();
    let first = components.next()?.as_os_str().to_str()?.to_owned();
    let rest: Vec<&OsStr> = components.map(Component::as_os_str).collect();

    // `tests/<stem>.rs` (direct child).
    if rest.is_empty() {
        let name = first.strip_suffix(".rs")?.to_owned();
        let path = format!("tests/{name}.rs");
        return Some(manifest::IntegrationTestEntry { name, path });
    }

    // `tests/<suite>/mod.rs` — a suite root. Deeper files in that
    // suite are submodules and don't get their own entry.
    if matches!(rest.as_slice(), [only] if *only == OsStr::new("mod.rs")) {
        let path = format!("tests/{first}/mod.rs");
        return Some(manifest::IntegrationTestEntry { name: first, path });
    }

    None
}
