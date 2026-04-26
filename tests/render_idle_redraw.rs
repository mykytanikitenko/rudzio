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
use rudzio::output::events::{LifecycleEvent, PipeChunk, StdStream, TestId, TestState, TestStateKind};
use rudzio::output::render::{Drawer, running_line, running_output_lines, spawn_drawer};
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
        Self::spawn_with_size(None)
    }

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
        ColorPolicy, Duration, Harness, Instant, LifecycleEvent, PipeChunk, RUNNING_HEADER,
        StdStream, TestId, TestOutcome, TestState, TestStateKind, Vt100, running_line,
        running_output_lines, strip_ansi, thread,
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
        let started_at = Instant::now() - Duration::from_millis(220);

        let mut state = TestState {
            module_path: long_module,
            test_name: long_test_name,
            runtime_name: "tokio::Multithread",
            thread: thread::current().id(),
            started_at,
            kind: TestStateKind::Running,
            stdout_buffer: Vec::new(),
            stderr_buffer: Vec::new(),
            last_output_line: long_stdout.clone(),
            recent_output: vec![long_stdout],
        };
        // A second, multi-byte UTF-8 line — the byte-length is > the
        // char-count; an off-by-one width calc on bytes vs chars
        // would still make this overflow.
        state.recent_output.push("кириллица".repeat(40));

        // Sweep across realistic terminal widths. We include a
        // boundary case (cols = `RUNTIME_PREFIX_WIDTH + STATUS_TAG_WIDTH
        // + 1 + MIN_TRAILING_PAD + trailing-len`, the smallest width
        // that fits the framework overhead) plus the typical 80/100/
        // 120 terminals plus the user's reported ~95.
        for cols in [40, 60, 80, 95, 100, 120, 200] {
            let color = ColorPolicy::off();
            let row = running_line("tokio::Multithread", &state, color, cols);
            check_row_fits(&format!("running_line @ cols={cols}"), &row, cols)?;

            let height = 24;
            for (idx, line) in
                running_output_lines(&state, color, cols, height).iter().enumerate()
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

    fn run_repaint_scenario(cols: usize, height: usize) -> anyhow::Result<()> {
        let h = Harness::spawn_with_size(Some((cols, height)))?;
        let test_id = TestId::next();
        let drawer_owner_thread = h.drawer_thread.thread().id();

        h.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "rudzio::render_idle_redraw",
            test_name: "live_redraw_drops_running_header_when_all_slots_idle",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;
        for i in 0..6 {
            h.pipe_tx.send(PipeChunk {
                stream: StdStream::Stdout,
                bytes: format!("output line {i}\n").into_bytes(),
            })?;
        }
        thread::sleep(Duration::from_millis(180));

        h.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Passed {
                elapsed: Duration::from_millis(180),
            },
        })?;
        thread::sleep(Duration::from_millis(80));

        let captured = h.finish()?;

        let mut term = Vt100::new(cols, height);
        term.feed(captured.as_bytes());

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
    cols: usize,
    rows: Vec<String>,
    scrollback: Vec<String>,
    cur_row: usize,
    cur_col: usize,
    height: usize,
}

impl Vt100 {
    fn new(cols: usize, height: usize) -> Self {
        Self {
            cols,
            rows: vec![String::new(); height],
            scrollback: Vec::new(),
            cur_row: 0,
            cur_col: 0,
            height,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let s = std::str::from_utf8(bytes).expect("utf-8");
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    let _ = chars.next();
                    let mut params = String::new();
                    let mut final_byte = '\0';
                    while let Some(&ch) = chars.peek() {
                        if ch.is_ascii_digit() || ch == ';' {
                            params.push(ch);
                            let _ = chars.next();
                        } else if ('@'..='~').contains(&ch) {
                            final_byte = ch;
                            let _ = chars.next();
                            break;
                        } else {
                            break;
                        }
                    }
                    self.csi(&params, final_byte);
                }
                continue;
            }
            if c == '\n' {
                self.advance_row();
                self.cur_col = 0;
                continue;
            }
            if c == '\r' {
                self.cur_col = 0;
                continue;
            }
            self.put_char(c);
        }
    }

    fn put_char(&mut self, c: char) {
        if self.cur_col >= self.cols {
            // DECAWM auto-wrap: deferred wrap fires when the next
            // printing char arrives past the rightmost column.
            self.advance_row();
            self.cur_col = 0;
        }
        let row = &mut self.rows[self.cur_row];
        // Pad with spaces if we're inserting past the row's current
        // end (CSI cursor-up keeps cur_col high but the row may be
        // short).
        let row_chars = row.chars().count();
        if self.cur_col >= row_chars {
            for _ in row_chars..self.cur_col {
                row.push(' ');
            }
            row.push(c);
        } else {
            // Overwrite at column. Simple replace at byte level by
            // rebuild — this is test-only, perf doesn't matter.
            let mut rebuilt = String::new();
            for (i, ch) in row.chars().enumerate() {
                if i == self.cur_col {
                    rebuilt.push(c);
                } else {
                    rebuilt.push(ch);
                }
            }
            *row = rebuilt;
        }
        self.cur_col += 1;
    }

    fn advance_row(&mut self) {
        if self.cur_row + 1 < self.height {
            self.cur_row += 1;
        } else {
            // Scroll: top row leaves the viewport and lands in scrollback.
            let evicted = std::mem::take(&mut self.rows[0]);
            self.scrollback.push(evicted);
            for r in 0..(self.height - 1) {
                self.rows[r] = std::mem::take(&mut self.rows[r + 1]);
            }
            self.rows[self.height - 1].clear();
        }
    }

    fn csi(&mut self, params: &str, final_byte: char) {
        match final_byte {
            'A' => {
                let n = params.parse::<usize>().unwrap_or(1).max(1);
                self.cur_row = self.cur_row.saturating_sub(n);
            }
            'J' => {
                let mode = params.parse::<u32>().unwrap_or(0);
                if mode == 0 {
                    let row = &mut self.rows[self.cur_row];
                    let row_chars = row.chars().count();
                    if self.cur_col < row_chars {
                        let kept: String = row.chars().take(self.cur_col).collect();
                        *row = kept;
                    }
                    for r in (self.cur_row + 1)..self.height {
                        self.rows[r].clear();
                    }
                }
            }
            // 'm' (SGR), and everything else: ignore for our purposes.
            _ => {}
        }
    }
}

/// Strip ANSI CSI escape sequences (`ESC [ … final-byte`) so we can
/// count the *visible* width of a rendered row. Production output
/// uses dim/colour escapes for the runtime prefix, status tag, and
/// stdio hints; only the printing characters consume terminal cells.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // ESC; only handle the CSI form `ESC [ … <final 0x40-0x7e>`,
        // which covers every escape the renderer emits (SGR, cursor-
        // up, erase-in-display).
        if chars.next() != Some('[') {
            continue;
        }
        for ch in chars.by_ref() {
            if ('@'..='~').contains(&ch) {
                break;
            }
        }
    }
    out
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
