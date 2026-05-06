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
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ::rudzio::common::context::{Suite, Test};
use ::rudzio::runtime::futures::ThreadPool;
use ::rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use ::rudzio::runtime::{async_std, compio, embassy, smol};

/// The acknowledgement phrase the migrator's preflight gate requires
/// on stdin. Intentionally typo-laden: the test asserts byte-for-byte
/// equality with what the binary expects.
const ACK_PHRASE: &str = "I am not and idion and understand what I am doing in most cases at least";

#[cfg(any(test, rudzio_test))]
#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        PathBuf, Test, fs, git_init_commit, run_fixture, run_fixture_twice, run_migrate,
        setup_minimal_lib_crate,
    };
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_plain_sync_test() {
        run_fixture("plain_sync_test");
    }
    */
    #[rudzio::test]
    async fn golden_plain_sync_test(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("plain_sync_test");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_cfg_lints_preserve_existing(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("cfg_lints_preserve_existing");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_cfg_attr_test_broadening(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("cfg_attr_test_broadening");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_cfg_attr_only_no_suite(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("cfg_attr_only_no_suite");
        Ok(())
    }
    #[rudzio::test]
    async fn migrator_is_idempotent_on_already_migrated_crate(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture_twice("plain_sync_test");
        Ok(())
    }

    /// Pristine-manifest invariant: no `expected/Cargo.toml` anywhere
    /// in the fixture tree may contain a `[target."cfg(rudzio_test)".dependencies]`
    /// block or any `[target.*] rudzio_test` mirror. The only place
    /// `rudzio_test` is allowed to appear in a generated Cargo.toml is
    /// the `check-cfg` entry under `[lints.rust]`. Regression guard for
    /// the mirror removal (commit 495b42f) — catches accidental
    /// reintroduction of the target.cfg mirror by a future migrator
    /// change.
    #[rudzio::test]
    async fn no_cfg_rudzio_test_mirror_in_any_fixture_cargo_toml(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        use std::fs;
        use std::path::Path;
        fn walk_cargo_tomls(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(read_dir) = fs::read_dir(dir) else {
                return;
            };
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk_cargo_tomls(&path, out);
                    continue;
                }
                if path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml") {
                    out.push(path);
                }
            }
        }
        let fixtures_root = ::rudzio::manifest_dir!().join("fixtures");
        let mut manifests = Vec::new();
        walk_cargo_tomls(&fixtures_root, &mut manifests);
        anyhow::ensure!(
            !manifests.is_empty(),
            "no Cargo.toml files discovered under {}",
            fixtures_root.display()
        );
        let mut offenders: Vec<String> = Vec::new();
        for path in manifests {
            // Only check /expected/ trees — input/ trees are raw user
            // shapes and may legitimately carry anything.
            if !path.to_string_lossy().contains("/expected/") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for (line_idx, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                // The mirror block header — exact match.
                if trimmed.starts_with("[target") && trimmed.contains("cfg(rudzio_test)") {
                    offenders.push(format!(
                        "{}:{}: found target.cfg(rudzio_test) mirror block",
                        path.display(),
                        line_idx.saturating_add(1)
                    ));
                }
            }
        }
        anyhow::ensure!(
            offenders.is_empty(),
            "pristine-manifest invariant broken — {} offender(s):\n{}",
            offenders.len(),
            offenders.join("\n")
        );
        Ok(())
    }

    /// Complement to the pristine-manifest check: confirm migrated
    /// Cargo.tomls don't acquire a `anyhow = ...` entry just because
    /// the migrator ran. The old rewriter forced anyhow on every crate
    /// that returned `anyhow::Result<()>`; the current rewriter leaves
    /// user signatures alone so no `anyhow` line should land in
    /// `[dev-dependencies]` unless the user's own `[dependencies]` or
    /// `[dev-dependencies]` already had it.
    #[rudzio::test]
    async fn migrator_does_not_add_anyhow_to_dev_dependencies(_ctx: &Test) -> anyhow::Result<()> {
        use std::fs;
        let fixtures_root = ::rudzio::manifest_dir!().join("fixtures");
        for entry in fs::read_dir(&fixtures_root)? {
            let fixture = entry?.path();
            if !fixture.is_dir() {
                continue;
            }
            let input_manifest = fixture.join("input/Cargo.toml");
            let expected_manifest = fixture.join("expected/Cargo.toml");
            if !input_manifest.exists() || !expected_manifest.exists() {
                continue;
            }
            let input_has_anyhow =
                fs::read_to_string(&input_manifest).is_ok_and(|text| text.contains("anyhow"));
            if input_has_anyhow {
                continue; // user's own dep — migrator must not touch it
            }
            let expected_text = fs::read_to_string(&expected_manifest)?;
            anyhow::ensure!(
                !expected_text.contains("anyhow"),
                "fixture `{}` gained an `anyhow` entry after migration but had none in input:\n{}",
                fixture.file_name().unwrap_or_default().to_string_lossy(),
                expected_text
            );
        }
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_test_context_migration() {
        run_fixture("test_context_migration");
    }
    */
    #[rudzio::test]
    async fn golden_test_context_migration(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("test_context_migration");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_tokio_default() {
        run_fixture("tokio_default");
    }
    */
    #[rudzio::test]
    async fn golden_tokio_default(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("tokio_default");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_tokio_multi_thread() {
        run_fixture("tokio_multi_thread");
    }
    */
    #[rudzio::test]
    async fn golden_tokio_multi_thread(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("tokio_multi_thread");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_tokio_current_thread() {
        run_fixture("tokio_current_thread");
    }
    */
    #[rudzio::test]
    async fn golden_tokio_current_thread(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("tokio_current_thread");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_ignore_variants() {
        run_fixture("ignore_variants");
    }
    */
    #[rudzio::test]
    async fn golden_ignore_variants(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("ignore_variants");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_should_panic_warn() {
        run_fixture("should_panic_warn");
    }
    */
    #[rudzio::test]
    async fn golden_should_panic_warn(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("should_panic_warn");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_bench_warn() {
        run_fixture("bench_warn");
    }
    */
    #[rudzio::test]
    async fn golden_bench_warn(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("bench_warn");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_rstest_skipped() {
        run_fixture("rstest_skipped");
    }
    */
    #[rudzio::test]
    async fn golden_rstest_skipped(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("rstest_skipped");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_plain_async_test(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("plain_async_test");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_async_std_test(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("async_std_test");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_compio_test(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("compio_test");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_result_returning_test(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("result_returning_test");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_dry_run(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("dry_run");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_no_shared_runner_flag(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("no_shared_runner_flag");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_no_preserve_originals_flag(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("no_preserve_originals_flag");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_nested_modules(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("nested_modules");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_integration_file(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("integration_file");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_test_context_sync_variant(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("test_context_sync_variant");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_test_context_unresolvable(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("test_context_unresolvable");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_workspace_crate(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("workspace_crate");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_result_returning_block_body(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("result_returning_block_body");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_tests_only_flag(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("tests_only_flag");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_existing_test_harness_flip(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("existing_test_harness_flip");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_tests_subdir_layout(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("tests_subdir_layout");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_test_context_nested_impl(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("test_context_nested_impl");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_ambassador_verbatim(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("ambassador_verbatim");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_ambassador_verbatim_with_tests(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("ambassador_verbatim_with_tests");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_cfg_test_with_expect_attr(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("cfg_test_with_expect_attr");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_lib_unit_tests_harness_false(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("lib_unit_tests_harness_false");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_nested_parent_mod_not_promoted(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("nested_parent_mod_not_promoted");
        Ok(())
    }
    #[rudzio::test]
    async fn golden_crate_with_bins(_ctx: &Test) -> anyhow::Result<()> {
        run_fixture("crate_with_bins");
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_dirty_tree_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        setup_minimal_lib_crate(path, "dirty_tree").expect("setup");
        git_init_commit(path).expect("git commit");
        // Make the tree dirty *after* the commit so the tool sees
        // uncommitted changes.
        fs::write(path.join("src/lib.rs"), b"// uncommitted edit\n")
            .expect("dirty write");

        let output = run_migrate(path, &["--dry-run"], "").expect("spawn");
        assert_eq!(
            output.status.code(),
            Some(1_i32),
            "expected exit 1 for dirty tree; got {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("refusing to run because the working tree has uncommitted changes"),
            "expected dirty-tree disclaimer; got:\n{stdout}"
        );
        assert!(
            stdout.contains("This tool is not going to do any magic"),
            "expected best-effort disclaimer; got:\n{stdout}"
        );
        // Nothing must have been written — the whole point of the gate.
        assert!(
            !path.join("src/lib.rs.backup_before_migration_to_rudzio").exists(),
            "backup should not have been created for a dirty tree refusal",
        );
    }
    */
    #[rudzio::test]
    async fn golden_dirty_tree_refusal(_ctx: &Test) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        setup_minimal_lib_crate(path, "dirty_tree").expect("setup");
        git_init_commit(path).expect("git commit");
        fs::write(path.join("src/lib.rs"), b"// uncommitted edit\n").expect("dirty write");
        let output = run_migrate(path, &["--dry-run"], "").expect("spawn");
        assert_eq!(
            output.status.code(),
            Some(1_i32),
            "expected exit 1 for dirty tree; got {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("refusing to run because the working tree has uncommitted changes"),
            "expected dirty-tree disclaimer; got:\n{stdout}"
        );
        assert!(
            stdout.contains("This tool is not going to do any magic"),
            "expected best-effort disclaimer; got:\n{stdout}"
        );
        assert!(
            !path
                .join("src/lib.rs.backup_before_migration_to_rudzio")
                .exists(),
            "backup should not have been created for a dirty tree refusal",
        );
        Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn golden_wrong_acknowledgement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        setup_minimal_lib_crate(path, "wrong_ack").expect("setup");
        git_init_commit(path).expect("git commit");

        // A subtly-different phrase: corrected typo + "an" instead of "and".
        let wrong = "I am not an idiot and understand what I am doing in most cases at least\n";
        let output = run_migrate(path, &[], wrong).expect("spawn");
        assert_eq!(
            output.status.code(),
            Some(1_i32),
            "expected exit 1 for wrong ack; got {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("acknowledgement did not match"),
            "expected ack-mismatch abort message; got:\n{stdout}"
        );
        assert!(
            !path.join("src/lib.rs.backup_before_migration_to_rudzio").exists(),
            "backup should not have been created for a wrong ack",
        );
    }
    */
    #[rudzio::test]
    async fn golden_wrong_acknowledgement(_ctx: &Test) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        setup_minimal_lib_crate(path, "wrong_ack").expect("setup");
        git_init_commit(path).expect("git commit");
        let wrong = "I am not an idiot and understand what I am doing in most cases at least\n";
        let output = run_migrate(path, &[], wrong).expect("spawn");
        assert_eq!(
            output.status.code(),
            Some(1_i32),
            "expected exit 1 for wrong ack; got {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("acknowledgement did not match"),
            "expected ack-mismatch abort message; got:\n{stdout}"
        );
        assert!(
            !path
                .join("src/lib.rs.backup_before_migration_to_rudzio")
                .exists(),
            "backup should not have been created for a wrong ack",
        );
        Ok(())
    }
}
/// Run the migrator binary against `migrate/fixtures/<name>/input/` in
/// a tempdir, then assert the resulting tree matches
/// `migrate/fixtures/<name>/expected/` byte-for-byte. Picks up optional
/// `args.txt` and `stdin.txt` overrides from the fixture directory.
fn run_fixture(name: &str) {
    let fixtures_root = ::rudzio::manifest_dir!().join("fixtures");
    let input_dir = fixtures_root.join(name).join("input");
    let expected_dir = fixtures_root.join(name).join("expected");
    assert!(
        input_dir.exists(),
        "fixture input missing: {}",
        input_dir.display()
    );
    assert!(
        expected_dir.exists(),
        "fixture expected missing: {}",
        expected_dir.display()
    );
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
    let _: &mut Command = cmd
        .arg("--path")
        .arg(tempdir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for arg in &extra_args {
        let _: &mut Command = cmd.arg(arg);
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
    compare_trees(&expected_dir, tempdir.path());
}

/// Run [`run_fixture`]'s migration step twice against the same input
/// and assert both runs produce byte-identical trees matching
/// `expected/`. Simulates the workflow a user takes after a successful
/// migration: first run produces the migration + backup files, the user
/// deletes backups and commits, then re-runs the tool. The second run
/// must be a no-op — no further rewrites, no duplicated Cargo.toml
/// entries, no double-wrapped cfg attrs.
fn run_fixture_twice(name: &str) {
    let fixtures_root = ::rudzio::manifest_dir!().join("fixtures");
    let input_dir = fixtures_root.join(name).join("input");
    let expected_dir = fixtures_root.join(name).join("expected");
    let tempdir = tempfile::tempdir().expect("tempdir");
    copy_tree(&input_dir, tempdir.path()).expect("copy input");
    git_init_commit(tempdir.path()).expect("git init/commit");
    invoke_migrate(tempdir.path(), &[]);
    // Clean backups so the working tree is ready for a second run;
    // then commit the tool's changes so the clean-tree gate passes.
    delete_backups(tempdir.path()).expect("delete backups");
    git_commit_all(tempdir.path()).expect("git commit post-first-run");
    invoke_migrate(tempdir.path(), &[]);
    delete_backups(tempdir.path()).expect("delete backups after 2nd run");
    compare_trees(&expected_dir, tempdir.path());
}

/// Spawn the migrator binary with the standard ACK stdin script and
/// `extra_args`, panicking if the process exits non-zero. Used by
/// [`run_fixture_twice`] for both runs of the idempotency check.
fn invoke_migrate(root: &Path, extra_args: &[&str]) {
    let bin = env!("CARGO_BIN_EXE_rudzio-migrate");
    let stdin_script = format!("{ACK_PHRASE}\nn\n");
    let mut cmd = Command::new(bin);
    let _: &mut Command = cmd
        .arg("--path")
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for arg in extra_args {
        let _: &mut Command = cmd.arg(arg);
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
}

/// Walk `root` and remove every file whose name ends in
/// `.backup_before_migration_to_rudzio` — the per-file backups the
/// migrator drops before rewriting. Lets [`run_fixture_twice`] simulate
/// the post-migration cleanup the user is expected to do before a
/// second run.
fn delete_backups(root: &Path) -> io::Result<()> {
    for path in walk(root) {
        if path
            .to_str()
            .is_some_and(|name| name.ends_with(".backup_before_migration_to_rudzio"))
        {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// `git add -A` then `git commit --allow-empty -m post-migrate` inside
/// `root`. Inline `user.email` / `user.name` so sandboxes without a
/// global git identity still succeed.
fn git_commit_all(root: &Path) -> io::Result<()> {
    let add_status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["add", "-A"])
        .status()?;
    if !add_status.success() {
        return Err(io::Error::other("git add failed"));
    }
    let commit_status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "post-migrate",
        ])
        .status()?;
    if !commit_status.success() {
        return Err(io::Error::other("git commit failed"));
    }
    Ok(())
}

/// Recursively copy every file from `src` into `dst`, creating
/// intermediate directories. `walk` already enumerates the full tree;
/// this is a tiny non-`std::fs::copy_dir_all`-shaped substitute.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in walk(src) {
        let rel = entry
            .strip_prefix(src)
            .map_err(|err| io::Error::other(format!("strip_prefix: {err}")))?;
        let target = dst.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let _bytes = fs::copy(&entry, &target)?;
    }
    Ok(())
}
/// Depth-first walk of `root`. Returns absolute paths of every regular
/// file found, sorted so byte-for-byte tree comparisons stay stable.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
/// `git init` + `git add .` + initial commit inside `dir`. Used by
/// every fixture-driven test to satisfy the migrator's clean-tree
/// preflight gate.
fn git_init_commit(dir: &Path) -> io::Result<()> {
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
/// Run `git <args>` inside `dir`, returning an `io::Error::other`
/// when the process exits non-zero. Thin wrapper used by
/// [`git_init_commit`] for its three sequential git invocations.
fn run_git(dir: &Path, args: &[&str]) -> io::Result<()> {
    let status = Command::new("git").current_dir(dir).args(args).status()?;
    if !status.success() {
        return Err(io::Error::other(format!("git {args:?} failed")));
    }
    Ok(())
}
/// Build a throwaway single-crate layout with a trivial `src/lib.rs`
/// and a `rust-toolchain.toml` pinning the workspace's toolchain. Used
/// by the negative-path tests (`dirty_tree_refusal`,
/// `wrong_acknowledgement`) where the crate contents don't matter —
/// only the preflight flow does.
fn setup_minimal_lib_crate(path: &Path, package_name: &str) -> io::Result<()> {
    fs::create_dir_all(path.join("src"))?;
    fs::write(
        path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{package_name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n"
        ),
    )?;
    fs::write(
        path.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95.0\"\n",
    )?;
    fs::write(path.join("src/lib.rs"), "pub fn noop() {}\n")
}
/// Run the `rudzio-migrate` binary against `path` with the given extra
/// args and stdin input, and return the captured `Output`. Does not
/// panic on non-zero exit — callers inspect `output.status` and
/// stdout/stderr to assert the expected failure mode.
fn run_migrate(path: &Path, extra_args: &[&str], stdin_input: &str) -> io::Result<Output> {
    let bin = env!("CARGO_BIN_EXE_rudzio-migrate");
    let mut cmd = Command::new(bin);
    let _: &mut Command = cmd
        .arg("--path")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for arg in extra_args {
        let _: &mut Command = cmd.arg(arg);
    }
    let mut child = cmd.spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(stdin_input.as_bytes())?;
    }
    child.wait_with_output()
}
/// Walk `expected` and assert every file's bytes match the same
/// relative path under `actual`. Panics with a fixture diff dump if any
/// file differs — the only failure mode the golden suite has, and the
/// reason `clippy::panic` is allowed at file scope.
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
