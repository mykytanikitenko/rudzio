//! Top-level orchestration. Lives here so `src/main.rs` can stay a
//! trivial binary and the lib-based callers (integration tests, a
//! hypothetical library user) can drive the same flow.

use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
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
    let workspace_dep_names = collect_workspace_dep_names(&args.path);

    let mut report = report::Report::new();
    let emit_opts = emit::EmitOptions {
        default_runtime: args.runtime,
        preserve_originals: !args.no_preserve_originals,
        dry_run: args.dry_run,
        test_contexts: &test_contexts,
    };

    for pkg in &packages {
        let mut pkg_edits = manifest::ManifestEdits {
            workspace_dep_names: workspace_dep_names.clone(),
            ..manifest::ManifestEdits::default()
        };
        let mut pkg_had_conversions = false;
        // Suite roots (`tests/<suite>/mod.rs`) whose subtree had
        // files modified. The rewriter only appends `fn main` to a
        // binary-root file that itself changed; for subdir layouts
        // the mod.rs is wiring-only and stays unchanged, but the
        // binary still needs a `fn main` to link. Fixed up below.
        let mut suite_roots_needing_main: std::collections::BTreeSet<PathBuf> =
            std::collections::BTreeSet::new();
        let src_iter: Box<dyn Iterator<Item = &PathBuf>> = if args.tests_only {
            Box::new(std::iter::empty())
        } else {
            Box::new(pkg.src_files.iter())
        };
        for file in src_iter.chain(pkg.tests_files.iter()) {
            match emit::process_file(file, &emit_opts, &mut report) {
                Ok(Some(rewrite)) => {
                    pkg_had_conversions = true;
                    pkg_edits.runtimes.extend(rewrite.runtimes_used.iter().copied());
                    pkg_edits.needs_anyhow |= rewrite.needs_anyhow;
                    if file.starts_with(pkg.root.join("src")) {
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
                    report.warn(
                        file.clone(),
                        None,
                        format!("error processing file: {err:#}"),
                    );
                }
            }
        }
        // `#[rudzio::suite]` expansion emits `#[allow(unsafe_code)]`
        // (linkme registers via `#[link_section]`, which rustc
        // classifies as unsafe_code). On crates whose `src/lib.rs`
        // carries `#![forbid(unsafe_code)]` the inner allow conflicts
        // — `forbid` doesn't accept downstream `allow` overrides.
        // Demote `forbid` to `deny` when this package had any src
        // conversion, so the user's intent (no unsafe in their code)
        // is preserved while leaving room for the macro expansion.
        if pkg_edits.had_src_conversion && !args.dry_run {
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
        // With `[lib] harness = false` (set by manifest::apply when
        // had_src_conversion), Cargo stops linking libtest into the
        // lib's test target — and without a `fn main`, the target
        // won't link at all. Append `#[cfg(test)] #[rudzio::main]
        // fn main() {}` to src/lib.rs as the replacement entry
        // point. The cfg(test) gate keeps the fn out of library
        // consumers' builds.
        if pkg_edits.had_src_conversion && !args.dry_run {
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
        if !args.dry_run {
            for mod_rs in &suite_roots_needing_main {
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
        if want_shared_runner && !args.dry_run && pkg_had_conversions {
            let tests_main = pkg.root.join("tests").join("main.rs");
            // Lib crate name uses hyphen→underscore per rustc's
            // "hyphens in crate names are normalized" rule.
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
                    // `[[test]] main` Cargo entry — adding one on top
                    // of a manifest that may already describe the
                    // same target breaks the manifest. If the user
                    // already has rudzio wired up here they don't
                    // need another [[test]] block; if not, the
                    // warning points them at the right next step.
                    report.warn(
                        tests_main,
                        None,
                        "tests/main.rs already exists; leaving it and its [[test]] entry alone — add `#[rudzio::main] fn main() {}` yourself if the file doesn't already host one",
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

/// Walk upward from `start` looking for a Cargo.toml whose
/// `[workspace.dependencies]` table is non-empty, and return the
/// set of dep names declared there. When found we use
/// `{ workspace = true, ... }` for dep edits instead of pinning a
/// fresh version per crate.
fn collect_workspace_dep_names(start: &Path) -> std::collections::BTreeSet<String> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            if let Ok(source) = std::fs::read_to_string(&candidate) {
                if let Ok(doc) = source.parse::<toml_edit::DocumentMut>() {
                    if let Some(ws_deps) = doc
                        .as_table()
                        .get("workspace")
                        .and_then(|i| i.as_table())
                        .and_then(|t| t.get("dependencies"))
                        .and_then(|i| i.as_table())
                    {
                        return ws_deps.iter().map(|(k, _)| k.to_owned()).collect();
                    }
                }
            }
        }
        if !current.pop() {
            return std::collections::BTreeSet::new();
        }
    }
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
    let rest: Vec<&std::ffi::OsStr> = components.map(|c| c.as_os_str()).collect();
    // File IS the suite root; nothing to do here — the rewriter
    // handles fn main injection for files that did change.
    if rest.is_empty()
        || (rest.len() == 1 && rest[0] == std::ffi::OsStr::new("mod.rs"))
    {
        return None;
    }
    Some(tests_dir.join(&suite).join("mod.rs"))
}

/// Append `#[rudzio::main] fn main() {}` to `mod_rs` if the file
/// exists and doesn't already contain a `fn main`. Returns `Ok(true)`
/// if the file was rewritten, `Ok(false)` if a `fn main` was already
/// there (or the file didn't exist). Makes a backup before writing.
/// Rewrite `#![forbid(unsafe_code)]` → `#![deny(unsafe_code)]` in
/// the package's `src/lib.rs` when the lib hosts migrated suite
/// blocks. Returns `Ok(Some(path))` when the file was rewritten,
/// `Ok(None)` if there was nothing to do. String-level replace —
/// fast, safe, and the common shape is exact-match.
fn demote_forbid_unsafe_in_lib(pkg_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context as _;
    let lib_rs = pkg_root.join("src/lib.rs");
    if !lib_rs.is_file() {
        return Ok(None);
    }
    let source = std::fs::read_to_string(&lib_rs)
        .with_context(|| format!("reading {}", lib_rs.display()))?;
    let target = "#![forbid(unsafe_code)]";
    if !source.contains(target) {
        return Ok(None);
    }
    let new = source.replace(target, "#![deny(unsafe_code)]");
    let _bak = crate::backup::copy_before_write(&lib_rs)
        .with_context(|| format!("backing up {}", lib_rs.display()))?;
    std::fs::write(&lib_rs, &new)
        .with_context(|| format!("writing {}", lib_rs.display()))?;
    Ok(Some(lib_rs))
}

/// Append `#[cfg(test)] #[rudzio::main] fn main() {}` to `src/lib.rs`
/// so the lib's test target (now `harness = false`) has an entry
/// point. Returns `Ok(Some(path))` if appended, `Ok(None)` if lib.rs
/// doesn't exist or already declares a `main`. The cfg(test) gate is
/// critical — without it, binaries that depend on this lib would
/// collide at link time with their own `fn main`.
fn ensure_lib_has_rudzio_main(pkg_root: &Path) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context as _;
    let lib_rs = pkg_root.join("src/lib.rs");
    if !lib_rs.is_file() {
        return Ok(None);
    }
    let source = std::fs::read_to_string(&lib_rs)
        .with_context(|| format!("reading {}", lib_rs.display()))?;
    // Idempotency: if the file already has a `fn main`, don't
    // append another one. The check uses syn (rather than a text
    // `contains`) because doc comments legitimately mention
    // `#[rudzio::main]` in explanatory prose without making the
    // file rudzio-ready. Parse failure → fall through to the
    // append path; worst case the user gets a duplicate `fn main`
    // and a compile error that points straight at the fix.
    if let Ok(tree) = syn::parse_file(&source) {
        let has_main = tree.items.iter().any(|it| {
            matches!(it, syn::Item::Fn(f) if f.sig.ident == "main")
        });
        if has_main {
            return Ok(None);
        }
    }
    let _bak = crate::backup::copy_before_write(&lib_rs)
        .with_context(|| format!("backing up {}", lib_rs.display()))?;
    let appended = if source.ends_with('\n') {
        format!("{source}#[cfg(test)]\n#[::rudzio::main]\nfn main() {{}}\n")
    } else {
        format!("{source}\n#[cfg(test)]\n#[::rudzio::main]\nfn main() {{}}\n")
    };
    std::fs::write(&lib_rs, &appended)
        .with_context(|| format!("writing {}", lib_rs.display()))?;
    Ok(Some(lib_rs))
}

fn ensure_suite_root_has_main(mod_rs: &Path) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    if !mod_rs.is_file() {
        return Ok(false);
    }
    let source = std::fs::read_to_string(mod_rs)
        .with_context(|| format!("reading {}", mod_rs.display()))?;
    let tree = match syn::parse_file(&source) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    let has_main = tree.items.iter().any(|it| {
        matches!(it, syn::Item::Fn(f) if f.sig.ident == "main")
    });
    if has_main {
        return Ok(false);
    }
    let _bak = crate::backup::copy_before_write(mod_rs)
        .with_context(|| format!("backing up {}", mod_rs.display()))?;
    let appended = if source.ends_with('\n') {
        format!("{source}\n#[rudzio::main]\nfn main() {{}}\n")
    } else {
        format!("{source}\n\n#[rudzio::main]\nfn main() {{}}\n")
    };
    std::fs::write(mod_rs, appended)
        .with_context(|| format!("writing {}", mod_rs.display()))?;
    Ok(true)
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
    let rest: Vec<&std::ffi::OsStr> =
        components.map(|c| c.as_os_str()).collect();

    // `tests/<stem>.rs` (direct child).
    if rest.is_empty() {
        let name = first.strip_suffix(".rs")?.to_owned();
        let path = format!("tests/{name}.rs");
        return Some(manifest::IntegrationTestEntry { name, path });
    }

    // `tests/<suite>/mod.rs` — a suite root. Deeper files in that
    // suite are submodules and don't get their own entry.
    if rest.len() == 1 && rest[0] == std::ffi::OsStr::new("mod.rs") {
        let path = format!("tests/{first}/mod.rs");
        return Some(manifest::IntegrationTestEntry { name: first, path });
    }

    None
}
