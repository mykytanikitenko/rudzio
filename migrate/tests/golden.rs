//! Golden-test harness for rudzio-migrate.

#![allow(
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::expect_used,
    clippy::format_push_string,
    clippy::branches_sharing_code,
    clippy::manual_assert,
    reason = "test harness: panic-on-diff and Result-shaped helpers are intentional"
)]

//!
//! Each fixture directory under `migrate/fixtures/<scenario>/` has an
//! `input/` tree (a minimal Rust package as it would exist before
//! migration) and an `expected/` tree (same package in the shape the
//! tool is expected to produce). The harness copies `input/` to a
//! tempdir, initialises it as a git repo, runs the binary with the
//! acknowledgement phrase piped via stdin, and diffs the result
//! against `expected/` byte-for-byte.
//!
//! `args.txt` (optional) provides extra CLI args; the first line is
//! parsed whitespace-split. `stdin.txt` (optional) overrides the
//! default stdin script (ACK phrase + "n" to skip the shared-runner
//! scaffolding). Scenario dirs without either file use the defaults.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ACK_PHRASE: &str =
    "I am not and idion and understand what I am doing in most cases at least";

#[test]
fn golden_plain_sync_test() {
    run_fixture("plain_sync_test");
}

#[test]
fn golden_test_context_migration() {
    run_fixture("test_context_migration");
}

#[test]
fn golden_tokio_default() {
    run_fixture("tokio_default");
}

#[test]
fn golden_tokio_multi_thread() {
    run_fixture("tokio_multi_thread");
}

#[test]
fn golden_tokio_current_thread() {
    run_fixture("tokio_current_thread");
}

#[test]
fn golden_ignore_variants() {
    run_fixture("ignore_variants");
}

#[test]
fn golden_should_panic_warn() {
    run_fixture("should_panic_warn");
}

#[test]
fn golden_bench_warn() {
    run_fixture("bench_warn");
}

#[test]
fn golden_rstest_skipped() {
    run_fixture("rstest_skipped");
}

fn run_fixture(name: &str) {
    let fixtures_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let input_dir = fixtures_root.join(name).join("input");
    let expected_dir = fixtures_root.join(name).join("expected");
    assert!(input_dir.exists(), "fixture input missing: {}", input_dir.display());
    assert!(expected_dir.exists(), "fixture expected missing: {}", expected_dir.display());

    let tempdir = tempfile::tempdir().expect("tempdir");
    copy_tree(&input_dir, tempdir.path()).expect("copy input");
    git_init_commit(tempdir.path()).expect("git init/commit");

    let bin = env!("CARGO_BIN_EXE_rudzio-migrate");
    let args_file = fixtures_root.join(name).join("args.txt");
    let stdin_file = fixtures_root.join(name).join("stdin.txt");
    let extra_args: Vec<String> = if args_file.exists() {
        fs::read_to_string(&args_file)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    let stdin_script = if stdin_file.exists() {
        fs::read_to_string(&stdin_file).unwrap_or_default()
    } else {
        format!("{ACK_PHRASE}\nn\n")
    };

    let mut cmd = Command::new(bin);
    #[allow(clippy::shadow_unrelated)]
    {
        let _c = cmd.arg("--path").arg(tempdir.path());
        for a in &extra_args {
            let _c = cmd.arg(a);
        }
        let _c = cmd.stdin(Stdio::piped());
        let _c = cmd.stdout(Stdio::piped());
        let _c = cmd.stderr(Stdio::piped());
    }
    let mut child = cmd.spawn().expect("spawn rudzio-migrate");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin_script.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    if !output.status.success() {
        panic!(
            "rudzio-migrate exited with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    // Compare every non-backup file under expected/ against the tempdir.
    compare_trees(&expected_dir, tempdir.path());
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in walk(src) {
        let rel = entry
            .strip_prefix(src)
            .map_err(|e| std::io::Error::other(format!("strip_prefix: {e}")))?;
        let target = dst.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let _bytes = fs::copy(&entry, &target)?;
        }
    }
    Ok(())
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn git_init_commit(dir: &Path) -> std::io::Result<()> {
    run_git(dir, &["init", "-q"])?;
    run_git(dir, &["add", "."])?;
    run_git(
        dir,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    )?;
    Ok(())
}

fn run_git(dir: &Path, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new("git").current_dir(dir).args(args).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("git {args:?} failed")));
    }
    Ok(())
}

fn compare_trees(expected: &Path, actual: &Path) {
    let mut diffs = Vec::new();
    for entry in walk(expected) {
        let rel = entry.strip_prefix(expected).expect("strip");
        let actual_path = actual.join(rel);
        let expected_bytes = fs::read(&entry).unwrap_or_default();
        let actual_bytes = fs::read(&actual_path).unwrap_or_default();
        if expected_bytes != actual_bytes {
            diffs.push((rel.to_path_buf(), expected_bytes, actual_bytes));
        }
    }
    if !diffs.is_empty() {
        let mut msg = String::from("fixture output differs from expected:\n");
        for (rel, exp, act) in &diffs {
            msg.push_str(&format!(
                "--- {} (expected vs actual) ---\nEXPECTED:\n{}\nACTUAL:\n{}\n",
                rel.display(),
                String::from_utf8_lossy(exp),
                String::from_utf8_lossy(act),
            ));
        }
        panic!("{msg}");
    }
}
