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

use std::env::temp_dir;
use std::fs::{self, File, OpenOptions};
use std::mem::take;
use std::path::PathBuf;
use std::process;
use std::str;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Sender, bounded, unbounded};

use rudzio::common::context::Suite;
use rudzio::config::{Format, OutputMode};
use rudzio::output::color::Policy as ColorPolicy;
use rudzio::output::events::{
    LifecycleEvent, PipeChunk, StdStream, TestId, TestState, TestStateBuffers, TestStateIdent,
    TestStateKind,
};
use rudzio::output::render::{Drawer, running_line, running_output_lines, spawn_drawer};
use rudzio::runtime::async_std;
use rudzio::runtime::compio;
use rudzio::runtime::embassy;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::smol;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::suite::TestOutcome;

/// Pre-rendered "── running ──" banner the live region paints above
/// the active running rows. Kept as a string constant so post-completion
/// scrollback assertions can search for it byte-for-byte without
/// reimporting the renderer's private formatting helpers.
const RUNNING_HEADER: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} running \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";

/// Handles for driving a synthetic `Drawer` from a test. Drop the
/// `life_tx` + `shutdown_tx` and the drawer winds down; the test then
/// joins via [`Harness::finish`] and reads the captured bytes back.
struct Harness {
    /// Join handle for the spawned drawer thread; consumed in
    /// `finish` so the drawer can wind down before the file is read.
    drawer_thread: thread::JoinHandle<()>,
    /// Lifecycle event channel; sending on it is how a test
    /// announces synthetic `TestStarted`/`TestCompleted` to the drawer.
    life_tx: Sender<LifecycleEvent>,
    /// Path of the on-disk file the drawer writes its terminal bytes
    /// to; deleted by `finish` once the contents have been read back.
    path: PathBuf,
    /// Pipe-chunk channel; sending on it is how a test feeds
    /// synthetic stdout/stderr lines into the drawer.
    pipe_tx: Sender<PipeChunk>,
    /// Read handle on the same `path` the drawer writes to (a separate
    /// fd via `try_clone`); kept alive for the duration of the run so
    /// the file isn't unlinked under it.
    reader: File,
    /// Bounded shutdown signal; dropping the sender unblocks the
    /// drawer's outer select on the shutdown channel.
    shutdown_tx: Sender<()>,
}

impl Harness {
    /// Drop the synthetic channels, join the drawer thread, then
    /// slurp the captured terminal bytes back from the on-disk
    /// file. Removes the file after reading.
    fn finish(self) -> anyhow::Result<String> {
        let Self {
            drawer_thread,
            life_tx,
            path,
            pipe_tx,
            reader,
            shutdown_tx,
        } = self;
        drop(life_tx);
        drop(pipe_tx);
        drop(shutdown_tx);
        // Joining a panicked drawer would obscure the assertion error
        // we actually want to surface; ignore the join result.
        let _join_result = drawer_thread.join();
        drop(reader);
        let captured = fs::read_to_string(&path)?;
        let _removed = fs::remove_file(&path);
        Ok(captured)
    }

    /// Spawn a `Drawer` against an on-disk synthetic terminal at the
    /// drawer's natural detected size. Equivalent to
    /// `spawn_with_size(None)`.
    fn spawn() -> anyhow::Result<Self> {
        Self::spawn_with_size(None)
    }

    /// Spawn a `Drawer` against an on-disk synthetic terminal,
    /// optionally forcing `(cols, rows)` so width-sensitive tests can
    /// pin exact viewport geometry instead of inheriting whatever the
    /// host environment exposes.
    fn spawn_with_size(size: Option<(usize, usize)>) -> anyhow::Result<Self> {
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

        let mut drawer = Drawer::new(
            life_rx,
            pipe_rx,
            shutdown_rx,
            writer_for_drawer,
            OutputMode::Live,
            Format::Pretty,
            ColorPolicy::off(),
        );
        if let Some((cols, rows)) = size {
            drawer = drawer.with_size_override(cols, rows);
        }
        let drawer_thread = spawn_drawer(drawer)?;
        Ok(Self {
            drawer_thread,
            life_tx,
            path,
            pipe_tx,
            reader: writer,
            shutdown_tx,
        })
    }
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
])]
mod tests {
    use super::{
        ColorPolicy, Duration, Harness, Instant, LifecycleEvent, PipeChunk, RUNNING_HEADER,
        StdStream, TestId, TestOutcome, TestState, TestStateBuffers, TestStateIdent, TestStateKind,
        Vt100, running_line, running_output_lines, strip_ansi, thread,
    };

    /// Helper: assert that `row` fits inside `cols` visible columns and
    /// contains no embedded newlines. Wraps both invariants the live
    /// region depends on so the cursor-up clear can find every row it
    /// painted last tick.
    fn check_row_fits(label: &str, row: &str, cols: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            !row.contains('\n'),
            "{label}: rendered row contains an embedded newline:\n{row:?}",
        );
        let stripped = strip_ansi(row);
        let visible = stripped.chars().count();
        anyhow::ensure!(
            visible <= cols,
            "{label}: visible width {visible} > cols {cols}\n\
             a row wider than `cols` auto-wraps in the terminal, but \
             `last_live_rows` only counts logical lines, so the cursor-up \
             clear can't reach the wrap-overflow row — it stays in \
             scrollback as a stale `[RUN]` stripe. Rendered:\n{row:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn live_redraw_drops_running_header_when_all_slots_idle() -> anyhow::Result<()> {
        // Phase 1 — one in-flight test on a Multithread slot. The
        // drawer paints the per-slot status on every 50ms tick.
        let harness = Harness::spawn()?;
        let test_id = TestId::next();
        let drawer_owner_thread = harness.drawer_thread.thread().id();
        harness.life_tx.send(LifecycleEvent::TestStarted {
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
        harness.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Passed {
                elapsed: Duration::from_millis(100),
            },
        })?;

        // Phase 3 — quiet idle window. With REDRAW_INTERVAL = 50ms
        // we expect ~5 ticks here; the contract says they paint
        // nothing.
        thread::sleep(Duration::from_millis(280));

        let captured = harness.finish()?;

        let completion_idx = captured.find("[OK]").ok_or_else(|| {
            anyhow::anyhow!(
                "drawer never emitted the test-completion line; captured bytes:\n{captured}",
            )
        })?;
        let post_completion = captured.get(completion_idx..).ok_or_else(|| {
            anyhow::anyhow!("completion index out of bounds; captured bytes:\n{captured}")
        })?;

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
        let harness = Harness::spawn()?;
        let test_id = TestId::next();
        let drawer_owner_thread = harness.drawer_thread.thread().id();

        harness.life_tx.send(LifecycleEvent::TestStarted {
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
        harness.pipe_tx.send(PipeChunk::new(
            b"hello from synthetic test\n".to_vec(),
            StdStream::Stdout,
        ))?;
        // Allow at least one full redraw cycle (50ms tick).
        thread::sleep(Duration::from_millis(140));

        harness.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Passed {
                elapsed: Duration::from_millis(140),
            },
        })?;
        // A short idle window so we can split the capture cleanly.
        thread::sleep(Duration::from_millis(80));

        let captured = harness.finish()?;
        let completion_idx = captured.find("[OK]").ok_or_else(|| {
            anyhow::anyhow!(
                "drawer never emitted the test-completion line; captured bytes:\n{captured}",
            )
        })?;
        // Everything before the completion is the in-flight live
        // region (cleared + repainted in place; raw bytes accumulate
        // each tick). That's where we expect to find the running row.
        let pre_completion = captured.get(..completion_idx).ok_or_else(|| {
            anyhow::anyhow!("completion index out of bounds; captured bytes:\n{captured}")
        })?;

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

    /// Reproducer for the scrollback-stripe bug the user reported:
    ///
    /// > tokio::Multithread[RUN]     rudzio::render_idle_redraw::
    /// > live_redraw_drops_running_header_when_…
    /// > tokio::Multithread[RUN]     rudzio::render_idle_redraw::
    /// > live_redraw_drops_running_header_when_…   (× 8)
    ///
    /// When the running row is wider than the terminal, the terminal
    /// auto-wraps it onto a second viewport row but the drawer only
    /// counts the *logical* row. The next tick's `\x1b[NA\x1b[J`
    /// clear undercounts and lands on the wrap-overflow row, so the
    /// first row of every wrap escapes into scrollback as a stale
    /// `[RUN]` stripe.
    ///
    /// The invariant the renderer must hold is therefore:
    /// **every row painted by `running_line` / `running_output_lines`
    /// fits inside the configured terminal width**, regardless of
    /// test name length or stdout line length. We exercise it with
    /// the qualified name from the user's report (length ≈ 80) and a
    /// 200-char synthetic stdout line, against a range of widths.
    #[rudzio::test]
    fn live_region_rows_never_exceed_terminal_width() -> anyhow::Result<()> {
        let long_test_name = "live_redraw_drops_running_header_when_all_slots_idle";
        let long_module = "rudzio::render_idle_redraw";
        let long_stdout: String = "x".repeat(200);
        let started_at = Instant::now()
            .checked_sub(Duration::from_millis(220))
            .ok_or_else(|| anyhow::anyhow!("clock cannot wind back 220ms from now"))?;

        let mut state = TestState::new(
            TestStateIdent::new(
                long_module,
                "tokio::Multithread",
                started_at,
                long_test_name,
                thread::current().id(),
            ),
            TestStateBuffers::new(
                long_stdout.clone(),
                vec![long_stdout],
                Vec::new(),
                Vec::new(),
            ),
            TestStateKind::Running,
        );
        // A second, multi-byte UTF-8 line — the byte-length is > the
        // char-count; an off-by-one width calc on bytes vs chars
        // would still make this overflow.
        state
            .recent_output
            .push("\u{43a}\u{438}\u{440}\u{438}\u{43b}\u{43b}\u{438}\u{446}\u{430}".repeat(40));

        // Sweep across realistic terminal widths. We include a
        // boundary case (cols = `RUNTIME_PREFIX_WIDTH + STATUS_TAG_WIDTH
        // + 1 + MIN_TRAILING_PAD + trailing-len`, the smallest width
        // that fits the framework overhead) plus the typical 80/100/
        // 120 terminals plus the user's reported ~95.
        for cols in [40_usize, 60, 80, 95, 100, 120, 200] {
            let color = ColorPolicy::off();
            let row = running_line("tokio::Multithread", &state, color, cols);
            check_row_fits(&format!("running_line @ cols={cols}"), &row, cols)?;

            let height = 24_usize;
            for (idx, line) in running_output_lines(&state, color, cols, height)
                .iter()
                .enumerate()
            {
                check_row_fits(
                    &format!("running_output_lines[{idx}] @ cols={cols}"),
                    line,
                    cols,
                )?;
            }
        }
        Ok(())
    }

    /// End-to-end reproducer for the user's report: a running test
    /// with a long qualified name leaves stale `[RUN]` stripes in
    /// the terminal scrollback after the run finishes.
    ///
    /// We drive a real `Drawer` configured with a forced terminal
    /// size (40×10) so the long test name absolutely must be
    /// clipped to fit one row. Pipe a few stdout lines through it
    /// so the live region also exercises the output-stream path.
    /// Replay the captured byte stream into a tiny VT100 emulator
    /// and check that nothing scrolled into history contains
    /// `[RUN]`.
    ///
    /// Pre-fix this test fails with stripes in scrollback.
    /// Post-fix the entire scrollback is `[RUN]`-free; the only
    /// thing left at the end is the test-completion line in the
    /// visible region (going through `emit_completion_block`,
    /// which IS supposed to stay around).
    #[rudzio::test]
    fn live_region_repaint_leaves_no_run_stripes_in_scrollback() -> anyhow::Result<()> {
        // Sweep realistic terminal widths against a fixed viewport
        // height. Includes the user's reported ~95-col terminal and
        // the boundary case where the running row would otherwise
        // land at exactly `cols` chars (which can trigger DECAWM
        // auto-wrap on some terminals even when the logical width
        // matches).
        for &(cols, height) in &[
            (40_usize, 10_usize),
            (60, 10),
            (80, 12),
            (95, 12),
            (100, 12),
            (120, 12),
            (200, 12),
        ] {
            run_repaint_scenario(cols, height)?;
        }
        Ok(())
    }

    /// Drive one repaint scenario at `cols` × `height`: spawn a
    /// drawer, simulate one in-flight test that emits 6 stdout lines,
    /// let the drawer tick, complete the test, then replay the
    /// captured bytes into a Vt100 emulator and assert the scrollback
    /// is `[RUN]`-stripe-free.
    fn run_repaint_scenario(cols: usize, height: usize) -> anyhow::Result<()> {
        let harness = Harness::spawn_with_size(Some((cols, height)))?;
        let test_id = TestId::next();
        let drawer_owner_thread = harness.drawer_thread.thread().id();

        harness.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "rudzio::render_idle_redraw",
            test_name: "live_redraw_drops_running_header_when_all_slots_idle",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;
        for index in 0_u32..6_u32 {
            harness.pipe_tx.send(PipeChunk::new(
                format!("output line {index}\n").into_bytes(),
                StdStream::Stdout,
            ))?;
        }
        thread::sleep(Duration::from_millis(180));

        harness.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Passed {
                elapsed: Duration::from_millis(180),
            },
        })?;
        thread::sleep(Duration::from_millis(80));

        let captured = harness.finish()?;

        let mut term = Vt100::new(cols, height);
        term.feed(captured.as_bytes())?;

        let stripes: Vec<String> = term
            .scrollback
            .iter()
            .filter(|line| line.contains("[RUN]"))
            .cloned()
            .collect();
        anyhow::ensure!(
            stripes.is_empty(),
            "cols={cols}, height={height}: live-region repaint left {} stale `[RUN]` row(s) \
             in scrollback after the test finished — the cursor-up clear did not reach them, \
             so the user sees a marquee of stripes when they scroll back.\n\
             Stripes:\n{stripes:#?}\n\n\
             Full scrollback ({} rows):\n{:#?}",
            stripes.len(),
            term.scrollback.len(),
            term.scrollback,
        );
        Ok(())
    }
}

/// Tiny VT100-style emulator. Just enough to interpret what the
/// drawer writes: printing chars, `\n`, `\r`, cursor-up (`ESC [ N A`),
/// erase-in-display (`ESC [ J`), and DECAWM auto-wrap. Other CSIs
/// (SGR colour codes) are consumed and ignored. Anything that scrolls
/// off the top of the viewport lands in `scrollback`.
#[derive(Debug)]
struct Vt100 {
    /// Configured viewport width in columns; printing chars beyond
    /// `cols` triggers DECAWM auto-wrap onto the next row.
    cols: usize,
    /// Cursor column position within the current row.
    cur_col: usize,
    /// Cursor row position within the viewport.
    cur_row: usize,
    /// Configured viewport height in rows; rows past `height` push
    /// the topmost row into `scrollback`.
    height: usize,
    /// Logical viewport rows (always exactly `height` entries while
    /// the emulator is alive).
    rows: Vec<String>,
    /// Rows that have scrolled off the top of the viewport, in order
    /// of eviction. The post-run assertion scans this for stripes.
    scrollback: Vec<String>,
}

impl Vt100 {
    /// Advance the cursor down one row. If we were already on the
    /// bottom row, evict the topmost row into `scrollback` and shift
    /// the rest up, leaving an empty new bottom row.
    fn advance_row(&mut self) {
        if self.cur_row.saturating_add(1) < self.height {
            self.cur_row = self.cur_row.saturating_add(1);
            return;
        }
        if self.rows.is_empty() {
            return;
        }
        self.rows.rotate_left(1);
        let Some(last) = self.rows.last_mut() else {
            return;
        };
        self.scrollback.push(take(last));
    }

    /// Handle a parsed CSI sequence (`ESC [ params final_byte`).
    /// Implements the few escapes the drawer actually emits: `A`
    /// (cursor up) and `J` (erase-in-display from cursor onward).
    /// Other final bytes (SGR `m`, etc.) are ignored.
    fn csi(&mut self, params: &str, final_byte: char) {
        match final_byte {
            'A' => {
                let count = params.parse::<usize>().unwrap_or(1).max(1);
                self.cur_row = self.cur_row.saturating_sub(count);
            }
            'J' => {
                let mode = params.parse::<u32>().unwrap_or(0);
                if mode == 0 {
                    let Some(row) = self.rows.get_mut(self.cur_row) else {
                        return;
                    };
                    let row_chars = row.chars().count();
                    if self.cur_col < row_chars {
                        let kept: String = row.chars().take(self.cur_col).collect();
                        *row = kept;
                    }
                    let start = self.cur_row.saturating_add(1);
                    self.rows.iter_mut().skip(start).for_each(String::clear);
                }
            }
            // 'm' (SGR), and everything else: ignore for our purposes.
            _ => {}
        }
    }

    /// Feed a byte slice to the emulator. Parses CSI sequences,
    /// advances the cursor on `\n`/`\r`, and writes printable chars.
    /// Returns an error if `bytes` is not valid UTF-8.
    fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let text = str::from_utf8(bytes)?;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                if chars.peek() == Some(&'[') {
                    let _: Option<char> = chars.next();
                    let mut params = String::new();
                    let mut final_byte = '\0';
                    while let Some(&peeked) = chars.peek() {
                        if peeked.is_ascii_digit() || peeked == ';' {
                            params.push(peeked);
                            let _: Option<char> = chars.next();
                        } else if ('@'..='~').contains(&peeked) {
                            final_byte = peeked;
                            let _: Option<char> = chars.next();
                            break;
                        } else {
                            break;
                        }
                    }
                    self.csi(&params, final_byte);
                }
                continue;
            }
            if ch == '\n' {
                self.advance_row();
                self.cur_col = 0;
                continue;
            }
            if ch == '\r' {
                self.cur_col = 0;
                continue;
            }
            self.put_char(ch);
        }
        Ok(())
    }

    /// Construct a fresh emulator with `height` empty rows of `cols`
    /// nominal width. The cursor starts at row 0, col 0; scrollback
    /// starts empty.
    fn new(cols: usize, height: usize) -> Self {
        Self {
            cols,
            cur_col: 0,
            cur_row: 0,
            height,
            rows: vec![String::new(); height],
            scrollback: Vec::new(),
        }
    }

    /// Print one character at the current cursor position. If the
    /// cursor is past `cols`, DECAWM auto-wrap fires before writing
    /// (advance row, reset col to 0).
    fn put_char(&mut self, ch: char) {
        if self.cur_col >= self.cols {
            // DECAWM auto-wrap: deferred wrap fires when the next
            // printing char arrives past the rightmost column.
            self.advance_row();
            self.cur_col = 0;
        }
        let Some(row) = self.rows.get_mut(self.cur_row) else {
            return;
        };
        // Pad with spaces if we're inserting past the row's current
        // end (CSI cursor-up keeps cur_col high but the row may be
        // short).
        let row_chars = row.chars().count();
        if self.cur_col >= row_chars {
            for _ in row_chars..self.cur_col {
                row.push(' ');
            }
            row.push(ch);
        } else {
            // Overwrite at column. Simple replace at byte level by
            // rebuild — this is test-only, perf doesn't matter.
            let mut rebuilt = String::new();
            for (index, existing) in row.chars().enumerate() {
                if index == self.cur_col {
                    rebuilt.push(ch);
                } else {
                    rebuilt.push(existing);
                }
            }
            *row = rebuilt;
        }
        self.cur_col = self.cur_col.saturating_add(1);
    }
}

/// Strip ANSI CSI escape sequences (`ESC [ … final-byte`) so we can
/// count the *visible* width of a rendered row. Production output
/// uses dim/colour escapes for the runtime prefix, status tag, and
/// stdio hints; only the printing characters consume terminal cells.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        // ESC; only handle the CSI form `ESC [ … <final 0x40-0x7e>`,
        // which covers every escape the renderer emits (SGR, cursor-
        // up, erase-in-display).
        if chars.next() != Some('[') {
            continue;
        }
        for csi_byte in chars.by_ref() {
            if ('@'..='~').contains(&csi_byte) {
                break;
            }
        }
    }
    out
}

/// Mint a unique on-disk path for a synthetic terminal file. PID +
/// nanos + a process-local atomic counter so two parallel rudzio runs
/// (or two tests in the same process) never collide.
fn unique_terminal_path() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    temp_dir().join(format!("rudzio-render-test-{pid}-{nanos}-{seq}.log"))
}
