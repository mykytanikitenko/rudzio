//! Live-region rendering contracts pinned by direct-drive tests.
//!
//! Two ends of the same contract:
//!
//! 1. While a test is running, the live region must show the per-slot
//!    `[RUN]` row with its `↳` hint rows replaying recent stdout — the
//!    user wants real-time status + captured output, updated in place
//!    every 50ms.
//! 2. While every slot is idle (between tests, after the last test),
//!    the live region must collapse to nothing. No `── running ──`
//!    header, no `[IDLE]` rows. Painting them on every 50ms tick turns
//!    the region into a noisy scroll marquee announcing "running"
//!    while nothing is running.
//!
//! Both tests construct a `Drawer` directly via its public surface,
//! point its terminal handle at a real on-disk file, drive synthetic
//! lifecycle / pipe events, and then read the captured bytes back to
//! assert on what the drawer wrote.

use std::fs::{File, OpenOptions};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Sender, bounded, unbounded};

use rudzio::config::{Format, OutputMode};
use rudzio::output::color::ColorPolicy;
use rudzio::output::events::{LifecycleEvent, PipeChunk, StdStream, TestId};
use rudzio::output::render::{Drawer, spawn_drawer};
use rudzio::suite::TestOutcome;

const RUNNING_HEADER: &str = "──────────────────────── running ────────────────────────";

/// Handles for driving a synthetic `Drawer` from a test. Drop the
/// `life_tx` + `shutdown_tx` and the drawer winds down; the test then
/// joins via [`Harness::finish`] and reads the captured bytes back.
struct Harness {
    path: PathBuf,
    reader: File,
    life_tx: Sender<LifecycleEvent>,
    pipe_tx: Sender<PipeChunk>,
    shutdown_tx: Sender<()>,
    drawer_thread: thread::JoinHandle<()>,
}

impl Harness {
    fn spawn() -> anyhow::Result<Self> {
        let path = unique_terminal_path();
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let writer_for_drawer = writer.try_clone()?;

        let (life_tx, life_rx) = unbounded::<LifecycleEvent>();
        let (pipe_tx, pipe_rx) = unbounded::<PipeChunk>();
        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

        let drawer = Drawer::new(
            life_rx,
            pipe_rx,
            shutdown_rx,
            writer_for_drawer,
            OutputMode::Live,
            Format::Pretty,
            ColorPolicy::off(),
        );
        let drawer_thread = spawn_drawer(drawer)?;
        Ok(Self {
            path,
            reader: writer,
            life_tx,
            pipe_tx,
            shutdown_tx,
            drawer_thread,
        })
    }

    fn finish(self) -> anyhow::Result<String> {
        use std::io::{Read as _, Seek as _};
        let Self {
            path,
            mut reader,
            life_tx,
            pipe_tx,
            shutdown_tx,
            drawer_thread,
        } = self;
        drop(life_tx);
        drop(pipe_tx);
        drop(shutdown_tx);
        // Joining a panicked drawer would obscure the assertion error
        // we actually want to surface; ignore the join result.
        let _join_result = drawer_thread.join();
        let mut captured = String::new();
        let _seek = reader.seek(SeekFrom::Start(0))?;
        let _bytes_read = reader.read_to_string(&mut captured)?;
        let _removed = std::fs::remove_file(&path);
        Ok(captured)
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::tokio::Local::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::embassy::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::futures::ThreadPool::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::{
        Duration, Harness, Instant, LifecycleEvent, PipeChunk, RUNNING_HEADER, StdStream, TestId,
        TestOutcome, thread,
    };

    #[rudzio::test]
    fn live_redraw_drops_running_header_when_all_slots_idle() -> anyhow::Result<()> {
        // Phase 1 — one in-flight test on a Multithread slot. The
        // drawer paints the per-slot status on every 50ms tick.
        let h = Harness::spawn()?;
        let test_id = TestId::next();
        let drawer_owner_thread = h.drawer_thread.thread().id();
        h.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "synthetic::module",
            test_name: "demo",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;
        thread::sleep(Duration::from_millis(120));

        // Phase 2 — test completes. From this point onwards no slot
        // is active; subsequent ticks are pure idle ticks.
        h.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Passed {
                elapsed: Duration::from_millis(100),
            },
        })?;

        // Phase 3 — quiet idle window. With REDRAW_INTERVAL = 50ms
        // we expect ~5 ticks here; the contract says they paint
        // nothing.
        thread::sleep(Duration::from_millis(280));

        let captured = h.finish()?;

        let completion_idx = captured.find("[OK]").ok_or_else(|| {
            anyhow::anyhow!(
                "drawer never emitted the test-completion line; captured bytes:\n{captured}",
            )
        })?;
        let post_completion = &captured[completion_idx..];

        let stray = post_completion.matches(RUNNING_HEADER).count();
        anyhow::ensure!(
            stray == 0,
            "after `TestCompleted`, the live-region redraw must not paint the \
             `── running ──` header again — every slot is idle, so it is pure \
             noise. Got {stray} occurrence(s) post-completion.\n\
             Post-completion captured bytes (escapes shown literally):\n{post_completion:?}",
        );

        // Belt-and-braces: an `[IDLE]` row is also stale noise. The
        // header check above covers it transitively today (the row
        // never ships without the header), but pin it explicitly so a
        // future "row without header" regression still fails here.
        anyhow::ensure!(
            !post_completion.contains("[IDLE]"),
            "`[IDLE]` row leaked into the post-completion live region:\n{post_completion:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn live_redraw_shows_running_status_and_recent_stdout() -> anyhow::Result<()> {
        // The user-facing contract behind the noise cleanup: while a
        // test is in flight, the live region must show the `[RUN]`
        // tag, the qualified test name, and the most recent stdout
        // lines as `↳` hint rows. This is the "I want to see status
        // of the test in realtime with its stdout/stderr" guarantee.
        let h = Harness::spawn()?;
        let test_id = TestId::next();
        let drawer_owner_thread = h.drawer_thread.thread().id();

        h.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "synthetic::module",
            test_name: "with_output",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;
        // Push a complete stdout line. `handle_pipe` attributes it
        // to the only in-flight test, appends to `recent_output`,
        // and the next redraw streams it untruncated below the
        // running status row — that's the "test status line + live
        // stdio/stderr below it" guarantee.
        h.pipe_tx.send(PipeChunk {
            stream: StdStream::Stdout,
            bytes: b"hello from synthetic test\n".to_vec(),
        })?;
        // Allow at least one full redraw cycle (50ms tick).
        thread::sleep(Duration::from_millis(140));

        h.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Passed {
                elapsed: Duration::from_millis(140),
            },
        })?;
        // A short idle window so we can split the capture cleanly.
        thread::sleep(Duration::from_millis(80));

        let captured = h.finish()?;
        let completion_idx = captured.find("[OK]").ok_or_else(|| {
            anyhow::anyhow!(
                "drawer never emitted the test-completion line; captured bytes:\n{captured}",
            )
        })?;
        // Everything before the completion is the in-flight live
        // region (cleared + repainted in place; raw bytes accumulate
        // each tick). That's where we expect to find the running row.
        let pre_completion = &captured[..completion_idx];

        anyhow::ensure!(
            pre_completion.contains("[RUN]"),
            "running row missing while the test was in flight; captured pre-completion:\n{pre_completion:?}",
        );
        // `qualified_test_name` runs `module_path` through
        // `normalize_module_path`, which strips the leading crate
        // segment — so `synthetic::module` becomes `module`. That's
        // why we look for `module::with_output`, not the raw input.
        anyhow::ensure!(
            pre_completion.contains("module::with_output"),
            "qualified test name missing from running row; captured pre-completion:\n{pre_completion:?}",
        );
        anyhow::ensure!(
            pre_completion.contains("hello from synthetic test"),
            "captured stdout line missing from live region below running row; captured pre-completion:\n{pre_completion:?}",
        );
        Ok(())
    }
}

fn unique_terminal_path() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rudzio-render-test-{pid}-{nanos}-{n}.log"))
}
