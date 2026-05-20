//! Reproducer for the recursive-cargo flock deadlock that surfaces
//! when the user has `[build] build-dir = "..."` set in their global
//! `~/.cargo/config.toml` (cargo 1.92+ feature).
//!
//! Linux-only: detection reads `/proc/locks` to identify the deadlock
//! signature directly, rather than inferring from a wall-clock timeout.
//! macOS would need a `lsof`-shelling variant; not implemented here.
#![cfg(target_os = "linux")]
//!
//! # Symptom
//!
//! `just test` → `cargo run -p cargo-rudzio -- test` would hang
//! indefinitely. `/proc/locks` showed the level-2 cargo (compiling the
//! auto-generated aggregator) holding an exclusive FLOCK on
//! `<build-dir>/.cargo-lock`, with the level-3 cargos spawned by the
//! aggregator's build.rs (the `expose_member_bins` calls inlined from
//! `cargo_rudzio::generate::BUILD_RS_HELPERS`) blocked forever on that
//! same flock. Cargo file locks have no timeout.
//!
//! # Root cause
//!
//! Cargo's `[build] build-dir` is a workspace-shared intermediate
//! artifacts dir, serialised by a single `.cargo-lock`. The aggregator's
//! build.rs spawns a nested `cargo build --bins --manifest-path <member>`
//! per bin-bearing member, but only overrides `CARGO_TARGET_DIR` to an
//! isolated per-OUT_DIR cache — not `CARGO_BUILD_BUILD_DIR`. The nested
//! cargo therefore inherits the outer's `[build.build-dir]` via the
//! project's `.cargo/config.toml` walk and contends for the same flock
//! the outer cargo is already holding.
//!
//! # Detection
//!
//! The test spawns `cargo-rudzio test` against a synthetic fixture, then
//! polls `/proc/locks` for the deadlock's exact kernel-visible signature:
//! at least one PID **in our process group** holds an FLOCK on some
//! inode, while at least one other PID **in the same process group**
//! is blocked waiting (`->`) on that same inode.
//!
//! Crucially, the detector requires the SAME `(inode, holder_pid,
//! waiter_pid)` triple to PERSIST across multiple samples spanning at
//! least `DEADLOCK_PERSISTENCE`. Cargo legitimately contends on its
//! own `$CARGO_HOME/.package-cache` flock when two cargo invocations
//! run concurrently (which is normal: bridge and aggregator build.rs
//! both spawn `cargo build --bins -p <consumer>` for the same
//! consumer). That contention resolves in a second or two as the
//! holding cargo finishes its registry-update phase. The bug we're
//! detecting is the recursive-cargo flock deadlock, which is
//! PERMANENT (cargo file locks have no timeout), so the persistence
//! check distinguishes the two cleanly.
//!
//! Pre-fix: bridge build.rs spawns nested `cargo build --bins -p
//! consumer` that inherits the outer's `[build] build-dir`,
//! contending with the L2 cargo on the build-dir's `.cargo-lock`.
//! L2 can't release the lock until bridge build.rs returns; bridge
//! build.rs can't return until its nested cargo succeeds. The
//! contention persists indefinitely; the persistence threshold trips
//! and the test fails with the exact `/proc/locks` evidence.
//!
//! Post-fix: `BUILD_RS_HELPERS` and `rudzio::build::expose_bins` set
//! `CARGO_BUILD_BUILD_DIR=<OUT_DIR>/rudzio-auto-bin-cache/<pkg>` on
//! every nested cargo spawn, so each gets its own build-dir flock,
//! never contending with the outer cargo. The cargos still
//! transiently contend on `.package-cache` but that resolves within
//! ~1s; the persistence threshold never trips; `cargo-rudzio test`
//! completes successfully.
//!
//! # Runtime matrix
//!
//! The poll loop awaits `ctx.sleep(...)` between detection samples, so
//! the body is genuinely async and exercises whichever runtime the
//! suite matrix dispatches to. The matrix mirrors `tests/runner.rs`'s
//! `runtime_ctx_api` block (the rudzio-wide convention for
//! runtime-spanning tests): every runtime cargo-rudzio's dev-deps
//! enable gets an entry.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
use tempfile::TempDir;

/// One observed contention event: which inode is contested and which
/// PIDs are holding vs. waiting. The polling loop compares successive
/// observations to require persistence before declaring deadlock.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContentionEvent {
    /// PIDs in our process group currently HOLDING an FLOCK on
    /// `inode`. Sorted ascending so equality across samples is
    /// order-independent.
    holders: Vec<i32>,
    /// Kernel `major:minor:inode` triple of the contended file.
    inode: String,
    /// PIDs in our process group currently BLOCKED WAITING on
    /// `inode`. Sorted ascending.
    waiters: Vec<i32>,
}

/// One row of `/proc/locks` we care about: a Linux file lock entry,
/// either held by some PID or with some PID blocked waiting on it.
#[derive(Debug, Clone)]
struct FlockEntry {
    /// The kernel-printed `major:minor:inode` triple. We treat it as
    /// an opaque key: same file = same triple, distinct files =
    /// distinct triples, which is all the deadlock check needs.
    inode: String,
    /// The PID that either owns this lock (when `waiting` is false)
    /// or is blocked waiting for it (when `waiting` is true).
    pid: i32,
    /// `false` for a row that **holds** the lock; `true` for a row
    /// that is **blocked waiting** on it (the `->` prefix in
    /// `/proc/locks`).
    waiting: bool,
}

/// Workspace root of the rudzio repo this test lives in.
///
/// `cargo-rudzio`'s manifest dir is `<workspace_root>/cargo-rudzio`;
/// joining `..` resolves to the workspace root where the `rudzio`
/// crate itself lives. The synthetic fixture references `rudzio` by
/// `path = <this>` so the test exercises the in-tree code, not the
/// published version.
///
/// `rudzio::manifest_dir!()` (not bare `env!("CARGO_MANIFEST_DIR")`)
/// is required because this test file is `#[path]`-included into the
/// cargo-rudzio aggregator crate when run via `cargo rudzio test`,
/// where `env!("CARGO_MANIFEST_DIR")` would resolve to
/// `<target>/rudzio-auto-runner/`, NOT `cargo-rudzio/`. The macro
/// consults rudzio's per-member registry first and returns the
/// original member's manifest dir; it falls back to `env!(...)` only
/// when run via plain `cargo test --test main` (where both paths are
/// equivalent).
///
/// `.join("..")` over `.parent()` stays infallible: cargo always sets
/// the manifest dir to an absolute multi-component path so `parent()`
/// would never return `None` in practice, but the `Option` would
/// still need an explicit fallback under `clippy::expect_used`.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Read everything currently buffered on `child`'s captured stdout and
/// stderr. Used for the failure diagnostic on both the timeout and the
/// non-zero-exit paths.
fn capture_output(child: &mut Child) -> (String, String) {
    use std::io::Read as _;
    let mut out = String::new();
    let mut err = String::new();
    if let Some(stream) = child.stdout.as_mut() {
        drop(stream.read_to_string(&mut out));
    }
    if let Some(stream) = child.stderr.as_mut() {
        drop(stream.read_to_string(&mut err));
    }
    (out, err)
}

/// Materialise the synthetic single-member workspace at `root`:
///
/// - `Cargo.toml` declaring `consumer` as the only workspace member,
///   registering `rudzio` as a path dep at `rudzio_root`.
/// - `.cargo/config.toml` setting `[build] build-dir = <build_dir>` so
///   the deadlock condition (shared build-dir flock) is in place.
/// - `consumer/`: a hybrid lib+bin crate.
///   * `src/lib.rs` carries the `// rudzio_test` marker and one
///     `#[rudzio::test]`, so cargo-rudzio detects it as a bridged
///     member (`has_src_rudzio_suite = true`).
///   * `src/main.rs` is a no-op bin, so cargo-rudzio populates
///     `member.bin_names = ["consumer"]`.
///
/// The bridged-AND-bin-bearing combination is the precise shape that
/// makes `cargo_rudzio::generate::build_bridge_build_rs` take the
/// bin-bearing branch (line 1944), inline `BUILD_RS_HELPERS`, and emit
/// the `expose_member_bins("consumer", ...)` call that spawns the
/// nested `cargo build --bins`. Real-world analogues in the rudzio
/// workspace itself: `cargo-rudzio` and `rudzio-migrate`.
fn write_fixture(root: &Path, rudzio_root: &Path, build_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo").join("config.toml"),
        format!("[build]\nbuild-dir = \"{}\"\n", build_dir.display()),
    )?;

    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\" }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;

    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[bin]]\n\
         name = \"consumer\"\n\
         path = \"src/main.rs\"\n\
         test = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true, features = [\"common\", \"runtime-tokio-multi-thread\"] }\n\
         anyhow = \"=1.0.102\"\n",
    )?;
    // `// rudzio_test` marker is what `detect_src_rudzio_suite` looks
    // for. Combined with the `[[bin]]` target above, the bridge crate's
    // build.rs takes the `BUILD_RS_HELPERS` branch and inlines the
    // `expose_member_bins("consumer", ..., &["consumer"])` call that
    // spawns the nested `cargo build --bins` we are testing.
    //
    // `#[cfg(any(test, rudzio_test))]` (NOT bare `#[cfg(test)]`) is
    // mandatory here: cargo-rudzio compiles the bridge under
    // `--cfg rudzio_test` but `cfg!(test)` is FALSE during a normal
    // `cargo build`. With bare `#[cfg(test)]` the `mod tests` block
    // is excluded, no `#[rudzio::test]` entries register into
    // `linkme::distributed_slice TEST_TOKENS`, and the aggregator
    // runner panics at startup on `slice::from_raw_parts(null, 0)`
    // when it tries to iterate the empty slice.
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn references_own_bin() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20// `env!(CARGO_BIN_EXE_consumer)` resolves at compile time only when\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20// the bridge's build.rs has successfully run\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20// `expose_member_bins(\"consumer\", ...)` -- i.e. the deadlock did\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20// not strike. The test exists purely to force that codegen.\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20let _: &str = env!(\"CARGO_BIN_EXE_consumer\");\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("main.rs"),
        "fn main() {}\n",
    )?;

    Ok(())
}

/// Set of PIDs whose `/proc/<pid>/stat` reports `pgrp == target_pgid`.
///
/// `/proc/<pid>/stat` format: `pid (comm) state ppid pgrp session ...`
/// where `comm` is parenthesised and may contain spaces/parens; the
/// last `)` in the line is the closing one (the kernel writes exactly
/// one pair). Everything after that is whitespace-separated, so the
/// third post-`)` field is `pgrp`.
fn pids_in_pgroup(target_pgid: i32) -> io::Result<HashSet<i32>> {
    let mut result = HashSet::new();
    for dir_entry_result in fs::read_dir("/proc")? {
        let dir_entry = dir_entry_result?;
        let Some(pid) = dir_entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        // Process can exit between readdir and read; treat that as
        // "not in our group" rather than propagating the ENOENT.
        let Ok(stat) = fs::read_to_string(dir_entry.path().join("stat")) else {
            continue;
        };
        let Some(close_idx) = stat.rfind(')') else {
            continue;
        };
        let Some(after_idx) = close_idx.checked_add(1) else {
            continue;
        };
        let Some(after_comm) = stat.get(after_idx..) else {
            continue;
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // Post-`)` fields: [0]=state, [1]=ppid, [2]=pgrp.
        let Some(pgrp_str) = fields.get(2) else {
            continue;
        };
        let Ok(pgrp) = pgrp_str.parse::<i32>() else {
            continue;
        };
        if pgrp == target_pgid {
            // `HashSet::insert` returns bool (was-new); we don't care.
            // `drop` on `Copy` is a no-op (clippy::drop_copy), bare
            // `let _` is forbidden (clippy::let_underscore), so an
            // explicit named-underscore binding it is.
            let _was_new: bool = result.insert(pid);
        }
    }
    Ok(result)
}

/// Parse `/proc/locks` into the FLOCK rows we care about.
///
/// Two row shapes (per `fs/locks.c::lock_get_status`):
///
/// ```text
///   N:    FLOCK ADVISORY WRITE PID maj:min:ino START END
///   N: -> FLOCK ADVISORY WRITE PID maj:min:ino START END
/// ```
///
/// The second form (`->`) is a process **blocked waiting** for the
/// lock held by some PID in the first form on the same inode. We only
/// care about `FLOCK` entries (cargo uses `flock(2)`, not POSIX
/// `fcntl` or OFD locks).
fn read_proc_locks() -> io::Result<Vec<FlockEntry>> {
    let raw = fs::read_to_string("/proc/locks")?;
    let mut entries = Vec::new();
    for line in raw.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let (waiting, kind_idx) = if tokens.get(1) == Some(&"->") {
            (true, 2_usize)
        } else {
            (false, 1_usize)
        };
        if tokens.get(kind_idx) != Some(&"FLOCK") {
            continue;
        }
        // Tokens after `FLOCK`: [+1]=ADVISORY, [+2]=WRITE/READ,
        // [+3]=PID, [+4]=maj:min:ino, [+5]=START, [+6]=END.
        let Some(pid_idx) = kind_idx.checked_add(3) else {
            continue;
        };
        let Some(inode_idx) = kind_idx.checked_add(4) else {
            continue;
        };
        let Some(pid_str) = tokens.get(pid_idx) else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };
        let Some(inode_str) = tokens.get(inode_idx) else {
            continue;
        };
        entries.push(FlockEntry {
            inode: (*inode_str).to_owned(),
            pid,
            waiting,
        });
    }
    Ok(entries)
}

/// Returns `Some(event)` if `/proc/locks` shows the exact
/// deadlock signature **within our process subtree**: at least one
/// PID in `pgid` holds an FLOCK on some inode, and at least one
/// **other** PID in the same pgid is blocked waiting on that same
/// inode.
///
/// A single observation is NOT sufficient evidence of a deadlock:
/// cargo legitimately contends on its `.package-cache` flock when
/// multiple cargo invocations run concurrently, and that contention
/// resolves within a second. The polling loop's persistence check
/// (same `ContentionEvent` across N consecutive samples) is what
/// distinguishes a true deadlock from normal cargo lock semantics.
fn detect_subtree_flock_contention(pgid: i32) -> io::Result<Option<ContentionEvent>> {
    let pids = pids_in_pgroup(pgid)?;
    if pids.is_empty() {
        return Ok(None);
    }
    let locks = read_proc_locks()?;

    // `BTreeMap` over `HashMap` so iteration order is deterministic
    // (`clippy::iter_over_hash_type`) and the resulting diagnostic
    // string is stable across runs.
    let mut by_inode: BTreeMap<String, Vec<&FlockEntry>> = BTreeMap::new();
    for lock in &locks {
        if pids.contains(&lock.pid) {
            by_inode.entry(lock.inode.clone()).or_default().push(lock);
        }
    }

    for (inode, lock_group) in &by_inode {
        let mut holders: Vec<i32> = lock_group
            .iter()
            .filter(|lock| !lock.waiting)
            .map(|lock| lock.pid)
            .collect();
        let mut waiters: Vec<i32> = lock_group
            .iter()
            .filter(|lock| lock.waiting)
            .map(|lock| lock.pid)
            .collect();
        if !holders.is_empty() && !waiters.is_empty() {
            holders.sort_unstable();
            waiters.sort_unstable();
            return Ok(Some(ContentionEvent {
                inode: inode.clone(),
                holders,
                waiters,
            }));
        }
    }
    Ok(None)
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use std::os::unix::process::CommandExt as _;
    use std::process::ExitStatus;

    use rudzio::context::Test as _;

    use super::{
        Child, Command, ContentionEvent, Duration, Instant, Path, PathBuf, Stdio, TempDir, Test,
        capture_output, detect_subtree_flock_contention, rudzio_source_root, write_fixture,
    };

    /// How long the SAME contention triple must persist before we
    /// declare a deadlock. Cargo's `.package-cache` flock legitimately
    /// contends for ~1s during registry updates; this threshold keeps
    /// us above that window while staying well below the safety bound.
    /// Tuned conservatively: the recursive-cargo deadlock is permanent
    /// (cargo file locks have no timeout), so any threshold from
    /// 3-30s would distinguish reliably.
    const DEADLOCK_PERSISTENCE: Duration = Duration::from_secs(5);

    /// Test outcome from the polling loop.
    enum Outcome {
        /// `/proc/locks` showed the same `(inode, holders, waiters)`
        /// contention triple across consecutive samples spanning at
        /// least `DEADLOCK_PERSISTENCE`. Wrapped value is the
        /// persisted event for the failure diagnostic.
        DeadlockDetected(ContentionEvent),
        /// The subprocess exited on its own. May be success or failure;
        /// the assertion stage inspects the wrapped `ExitStatus`.
        Exited(ExitStatus),
        /// Neither exited nor produced sustained flock contention
        /// within the safety bound. This should not normally happen:
        /// it indicates either a CI machine slow beyond our worst-case
        /// estimate, or a bug in detection. Surfaces with full stdio
        /// so the failure mode is investigable.
        SafetyBoundExpired,
    }

    /// Reproducer for the `BUILD_RS_HELPERS` recursive-cargo flock
    /// deadlock under shared `[build.build-dir]`.
    ///
    /// See module docs for the full mechanism. The verdict comes from
    /// `/proc/locks` direct inspection, not from a wall-clock timeout —
    /// pre-fix the kernel-visible "held by PID A in our pgroup, waited
    /// on by PID B in our pgroup" signature appears within seconds of
    /// the aggregator's build.rs running, and the test fails
    /// immediately with the exact PIDs and inode. Post-fix that
    /// signature never appears and `cargo-rudzio test` completes
    /// successfully against the fixture.
    #[rudzio::test]
    async fn cargo_rudzio_test_does_not_deadlock_under_shared_build_dir(
        ctx: &Test,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new()?;
        let root: &Path = workspace.path();
        let rudzio_root: PathBuf = rudzio_source_root();
        let build_dir: PathBuf = root.join("shared-build-dir");

        write_fixture(root, &rudzio_root, &build_dir)?;

        // Safety upper bound: only hit if BOTH detection AND
        // exit fail to fire. Pre-fix the deadlock signature shows up
        // within ~5-30s of process start; post-fix the full pipeline
        // (compile rudzio, build aggregator, run rudzio_tests)
        // completes well under this bound even cold. If the test
        // wedges for this long without contention OR exit, something
        // is wrong with detection or the host is pathologically slow,
        // and we want a visible failure rather than an infinite hang.
        let safety_bound = Duration::from_mins(5);
        let detection_interval = Duration::from_millis(200);

        let cargo_rudzio_bin: &str = env!("CARGO_BIN_EXE_cargo-rudzio");
        // `process_group(0)` makes the spawned cargo-rudzio the leader
        // of a new process group; nested `cargo run`/`cargo build`
        // grandchildren inherit it (they do not call `setpgid`
        // themselves). We use that pgid for two things: (1) the
        // `/proc/locks` detection scopes to PIDs in this group, so we
        // don't false-positive on unrelated cargo invocations
        // elsewhere on the host; (2) on detection or safety expiry we
        // `killpg` the entire group, reaping the deadlocked grandchild
        // cargos that would otherwise survive `Child::kill()` (which
        // only signals the immediate child).
        let mut child: Child = Command::new(cargo_rudzio_bin)
            .arg("test")
            .current_dir(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0_i32)
            .spawn()?;
        // `Child::id()` is `u32`; `libc::pid_t` is `i32`. Every real
        // Linux PID fits in `i32` (PIDs are 22-bit by default), but
        // `clippy::as_conversions` is forbidden, so use `try_from`.
        let pgid: libc::pid_t = libc::pid_t::try_from(child.id())?;

        let started = Instant::now();
        // Tracks the FIRST sample of the currently-persisting
        // contention triple. `None` means "no contention right now or
        // contention pattern changed since last sample". `Some((event,
        // first_seen))` means "this event has been observed
        // continuously since `first_seen`".
        let mut persisting: Option<(ContentionEvent, Instant)> = None;
        let outcome: Outcome = loop {
            if let Some(status) = child.try_wait()? {
                break Outcome::Exited(status);
            }

            let current = detect_subtree_flock_contention(pgid)?;
            persisting = match (persisting.take(), current) {
                (Some((prev, first_seen)), Some(curr)) if prev == curr => {
                    // Same triple as last sample. If it's been
                    // persisting long enough, declare deadlock.
                    if first_seen.elapsed() >= DEADLOCK_PERSISTENCE {
                        kill_pgroup(pgid);
                        drop(child.wait());
                        break Outcome::DeadlockDetected(prev);
                    }
                    Some((prev, first_seen))
                }
                (_, Some(curr)) => {
                    // New contention triple (or first observation
                    // after a gap). Reset the persistence timer.
                    Some((curr, Instant::now()))
                }
                (_, None) => {
                    // No contention. Clear state.
                    None
                }
            };

            if started.elapsed() >= safety_bound {
                kill_pgroup(pgid);
                drop(child.wait());
                break Outcome::SafetyBoundExpired;
            }

            // Runtime-agnostic sleep: routes through whichever runtime
            // the suite matrix dispatched this test under.
            ctx.sleep(detection_interval).await;
        };

        let (stdout, stderr) = capture_output(&mut child);
        let elapsed = started.elapsed();

        match outcome {
            Outcome::DeadlockDetected(event) => anyhow::bail!(
                "DEADLOCK detected via /proc/locks after {:.1}s against a workspace whose \
                 `.cargo/config.toml` sets `[build] build-dir = {}`. The same contention \
                 triple (inode {}, holders {:?}, waiters {:?}) persisted across samples \
                 spanning at least {}s, which is incompatible with the transient \
                 `.package-cache` contention that resolves in ~1s under normal cargo \
                 operation.\n\
                 \n\
                 Most likely cause: the consumer bridge's build.rs (from \
                 `cargo_rudzio::generate::BUILD_RS_HELPERS`) or `rudzio::build::expose_bins` \
                 spawned a nested `cargo build --bins --manifest-path <consumer>/Cargo.toml` \
                 that inherited the outer's `[build] build-dir` and blocked on the outer \
                 cargo's flock of `<build-dir>/.cargo-lock`. Fix: set \
                 `CARGO_BUILD_BUILD_DIR` to the per-OUT_DIR isolated cache on every nested \
                 cargo spawn (in both `BUILD_RS_HELPERS` and `rudzio::build::expose_bins`).\n\
                 \n\
                 --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
                elapsed.as_secs_f64(),
                build_dir.display(),
                event.inode,
                event.holders,
                event.waiters,
                DEADLOCK_PERSISTENCE.as_secs(),
                stdout,
                stderr,
            ),
            Outcome::SafetyBoundExpired => anyhow::bail!(
                "cargo-rudzio test neither exited nor exhibited a /proc/locks deadlock \
                 signature within the {}s safety bound. This is NOT the expected \
                 reproduction failure mode — either the host is pathologically slow \
                 (cold compile of rudzio + aggregator + consumer exceeded our worst-case \
                 estimate) or the `/proc/locks` detection has a bug. Investigate the \
                 captured output and re-run.\n\
                 --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
                safety_bound.as_secs(),
                stdout,
                stderr,
            ),
            Outcome::Exited(status) => {
                anyhow::ensure!(
                    status.success(),
                    "cargo-rudzio test exited non-zero against the deadlock fixture \
                     ({status}, elapsed {:.1}s). This is NOT a deadlock — the subprocess \
                     returned — but it failed for some other reason (compile error in the \
                     synthetic fixture, panic in cargo-rudzio itself, mis-resolved bin path, \
                     etc.). Investigate the captured output below.\n\
                     --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
                    elapsed.as_secs_f64(),
                    stdout,
                    stderr,
                );
                Ok(())
            }
        }
    }

    /// SIGKILL the entire process group identified by `pgid`.
    ///
    /// Used on deadlock detection and on safety-bound expiry: SIGTERM
    /// would queue behind the kernel flock-wait state on the
    /// deadlocked grandchild cargos, and `Child::kill()` only signals
    /// the immediate child, leaving the nested `cargo build --bins`
    /// grandchildren alive. `killpg` is the only mechanism that
    /// reaches them.
    ///
    /// Best-effort: if the group is already gone (the child exited
    /// between our `try_wait` and here), `killpg` returns -1 with
    /// `ESRCH` and we proceed with the diagnostic regardless.
    #[expect(
        unsafe_code,
        reason = "killpg is the only way to reap deadlocked grandchild cargos"
    )]
    fn kill_pgroup(pgid: libc::pid_t) {
        // SAFETY: `libc::killpg` is a thin syscall wrapper. It takes
        // a pgid and signal number, has no aliasing or memory
        // requirements, and is safe to call for any pgid this process
        // owns. We created this pgid via `process_group(0)` on the
        // spawned `cargo-rudzio` Command above, so we own it.
        let _ret: i32 = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
}
