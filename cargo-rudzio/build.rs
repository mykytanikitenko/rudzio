// Captures two build-time stamps so the produced binary can prove what
// it is and when it was built.
//
// 1. `CARGO_RUDZIO_BUILD_STAMP` — `<CARGO_PKG_VERSION>-<unix_epoch_seconds>`.
// 2. `CARGO_RUDZIO_BUILD_GIT_HEAD` / `CARGO_RUDZIO_BUILD_GIT_DIRTY` —
//    the git identity of the checkout that produced the binary.
//
// # Why the version+epoch stamp
//
// Bug #18: `<aggregator>/target/` is preserved across `cargo rudzio
// test` invocations. Cargo's fingerprinter keys on source content
// hashes — NOT on the producing binary's version. When `cargo-rudzio`
// is rebuilt and emits byte-identical aggregator source for an
// unchanged consumer workspace, cargo skips the relink and the OLD
// aggregator binary persists with OLD runner code. Behavioural fixes
// shipped in `cargo-rudzio` silently fail to take effect.
//
// The fix routes around cargo's fingerprinter: the aggregator subdir
// name includes a hash of this stamp (see `aggregator_subdir_name` in
// `src/generate.rs`). Different stamps → different subdir → fresh
// `target/` → fresh compile → fresh binary.
//
// # Why the git identity
//
// `env!("CARGO_BIN_EXE_cargo-rudzio")` resolves a path against the
// ambient `CARGO_TARGET_DIR`. A shared target dir can hand the e2e
// suite a binary built from a different HEAD than the test source. The
// suite re-checks this identity before every spawn (see
// `cargo-rudzio-test-helpers::assert_fresh_binary`): a binary whose
// reported HEAD differs from the live checkout HEAD fails loudly
// instead of silently running the wrong code.
//
// # git resolution
//
// Precedence for HEAD and dirty-state, highest first:
//
// 1. A non-empty `CARGO_RUDZIO_BUILD_GIT_HEAD` / `CARGO_RUDZIO_BUILD_GIT_DIRTY`
//    in the build script's environment. Builds from a source tree with
//    no `.git` (nix, published tarballs) pass the revision explicitly.
// 2. The `git` binary scoped to `CARGO_MANIFEST_DIR`, so worktrees,
//    packed refs, and detached HEAD all resolve correctly.
// 3. `"unknown"` — the build still succeeds, it just carries no
//    identity, and the freshness guard rejects it loudly.
//
// # Why runtime env reads
//
// `CARGO_MANIFEST_DIR` and `CARGO_PKG_VERSION` are read with
// `env::var`, never `env!`. When `cargo rudzio` bridges this crate the
// build script is spliced into a synthesized package and re-run after
// the bridge repoints those variables at the real member. A compile-time
// `env!` would freeze the synthesized package's directory (no `.git`)
// and version (`0.0.0`). Reading at runtime also lets the script compile
// standalone under `rustc`, which the stamp-precedence spec relies on.
//
// # rerun-if-changed
//
// Once any `rerun-if-changed` is emitted, cargo drops its implicit
// "rerun on any package file change" behaviour, so the source inputs
// are re-listed explicitly (`src`, `build.rs`, `Cargo.toml`) to keep
// the version+epoch stamp fresh on every source-touching edit. The git
// files (`HEAD`, `refs`, `packed-refs`) are watched in addition so a
// commit or pull refreshes the identity even when no `cargo-rudzio`
// source file changed — otherwise the identity would lag the checkout
// and the freshness guard would flag a false stale.
//
// NOTE: no `//!` inner doc comments and no `#![...]` inner attributes
// are permitted here. The build-script-propagation feature splices this
// file into `mod __member_build { include!("build.rs") }`, a context in
// which inner attributes are illegal.

use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

include!("build/source_fingerprint.rs");

/// Sentinel for "no git identity resolvable".
const UNKNOWN: &str = "unknown";

fn main() -> Result<(), Box<dyn Error>> {
    let version = env::var("CARGO_PKG_VERSION")
        .map_err(|err| format!("CARGO_PKG_VERSION is not set: {err}"))?;
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system clock before unix epoch: {err}"))?
        .as_secs();
    let stamp = format!("{version}-{epoch}");
    println!("cargo::rustc-env=CARGO_RUDZIO_BUILD_STAMP={stamp}");
    println!("cargo::rustc-env=CARGO_RUDZIO_BUILD_VERSION={version}");

    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR")
            .map_err(|err| format!("CARGO_MANIFEST_DIR is not set: {err}"))?,
    );
    let (head, dirty) = git_identity(&manifest_dir);
    println!("cargo::rustc-env=CARGO_RUDZIO_BUILD_GIT_HEAD={head}");
    println!("cargo::rustc-env=CARGO_RUDZIO_BUILD_GIT_DIRTY={dirty}");
    let source_hash = source_fingerprint::of(&manifest_dir);
    println!("cargo::rustc-env={}={}", source_fingerprint::ENV, source_hash);
    let profile = env::var("PROFILE").unwrap_or_else(|_| UNKNOWN.to_owned());
    println!("cargo::rustc-env=CARGO_RUDZIO_BUILD_PROFILE={profile}");

    // Keep the stamp fresh on source edits (Bug #18) and the identity
    // fresh on commit/pull. See the `rerun-if-changed` note above.
    for path in ["src", "build.rs", "Cargo.toml"] {
        println!("cargo::rerun-if-changed={path}");
    }
    for var in [
        "CARGO_RUDZIO_BUILD_GIT_HEAD",
        "CARGO_RUDZIO_BUILD_GIT_DIRTY",
    ] {
        println!("cargo::rerun-if-env-changed={var}");
    }
    if let Some(git_dir) = git_dir(&manifest_dir) {
        for watch in ["HEAD", "refs", "packed-refs"] {
            let path = git_dir.join(watch);
            if path.exists() {
                println!("cargo::rerun-if-changed={}", path.display());
            }
        }
    }

    Ok(())
}

/// Resolve the repository's `.git` directory for `dir`, handling the
/// git-worktree case where `.git` is a file (`gitdir: <path>`) rather
/// than a directory.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let gitdir = contents.strip_prefix("gitdir:")?.trim();
    Some(PathBuf::from(gitdir))
}

/// `(head_sha, dirty)` resolved per the precedence in the header: env
/// override, then `git` scoped to `dir`, then `"unknown"`.
fn git_identity(dir: &Path) -> (String, String) {
    let head = resolve_stamp(
        "CARGO_RUDZIO_BUILD_GIT_HEAD",
        git_cmd(dir, &["rev-parse", "HEAD"]),
    );
    let dirty = resolve_stamp(
        "CARGO_RUDZIO_BUILD_GIT_DIRTY",
        git_cmd(dir, &["status", "--porcelain", "--untracked-files=no"]).map(|out| {
            if out.is_empty() {
                "clean".to_owned()
            } else {
                "dirty".to_owned()
            }
        }),
    );
    (head, dirty)
}

/// A non-empty `override_var` in the environment wins over `git_value`;
/// with neither, the result is `"unknown"`.
fn resolve_stamp(override_var: &str, git_value: Option<String>) -> String {
    env::var(override_var)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or(git_value)
        .unwrap_or_else(|| UNKNOWN.to_owned())
}

/// Run `git -C <dir> <args>`, returning trimmed stdout, or `None` when
/// the command cannot run or exits non-zero (git missing / not a repo).
fn git_cmd(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}
