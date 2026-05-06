//! Live-region rendering contracts for `[BENCH]` tests.
//!
//! Three contracts pinned here:
//!
//! 1. While a bench test is in flight, the live region must show a
//!    progress bar, percent, `done/total`, p50/p95/cov in the trailing
//!    `<…>` block (adaptively shortened on narrow terminals), plus a
//!    mini-histogram below the running row.
//! 2. No `[BENCH]` row painted by `redraw_live_region` may exceed the
//!    terminal width. A row wider than `cols` auto-wraps and strands a
//!    stale stripe in scrollback when the cursor-up clear undercounts
//!    rows.
//! 3. The post-completion summary block (detailed stats, histogram)
//!    emitted by `emit_completion_block` is unchanged — progress
//!    events must not regress what the user sees after the bench
//!    finishes.

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

use rudzio::bench::{DistSummary, HISTOGRAM_BUCKETS, ProgressSnapshot, Report};
use rudzio::common::context::Suite;
use rudzio::config::{Format, OutputMode};
use rudzio::output::color::Policy as ColorPolicy;
use rudzio::output::events::{
    LifecycleEvent, PipeChunk, TestId, TestState, TestStateBuffers, TestStateIdent, TestStateKind,
};
use rudzio::output::render::{
    Drawer, bench_histogram_lines, bench_progress_trailing, running_line, spawn_drawer,
};
use rudzio::runtime::async_std;
use rudzio::runtime::compio;
use rudzio::runtime::embassy;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::suite::TestOutcome;

/// Handles for driving a synthetic bench `Drawer` from a test.
struct Harness {
    /// Join handle for the spawned drawer thread; consumed in
    /// `finish` so the drawer can wind down before the file is read.
    drawer_thread: thread::JoinHandle<()>,
    /// Lifecycle event channel; sending on it is how a test feeds
    /// `BenchProgress`/`TestStarted`/`TestCompleted` to the drawer.
    life_tx: Sender<LifecycleEvent>,
    /// Path of the on-disk file the drawer writes its terminal bytes
    /// to; deleted by `finish` once the contents have been read back.
    path: PathBuf,
    /// Pipe-chunk channel; bench tests don't normally use it but the
    /// drawer is constructed against the channel triple regardless.
    pipe_tx: Sender<PipeChunk>,
    /// Read handle on the same `path` the drawer writes to (separate
    /// fd via `try_clone`); kept alive for the run so the file isn't
    /// unlinked under it.
    reader: File,
    /// Bounded shutdown signal; dropping it unblocks the drawer's
    /// outer select on the shutdown channel.
    shutdown_tx: Sender<()>,
}

impl Harness {
    /// Drop the synthetic channels, join the drawer thread, then
    /// slurp the captured terminal bytes back from the on-disk file.
    /// Removes the file after reading.
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
        let _join_result = drawer_thread.join();
        drop(reader);
        let captured = fs::read_to_string(&path)?;
        let _removed = fs::remove_file(&path);
        Ok(captured)
    }

    /// Spawn a `Drawer` against an on-disk synthetic terminal,
    /// optionally forcing `(cols, rows)` so width-sensitive tests
    /// can pin exact viewport geometry.
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
])]
mod tests {
    use super::{
        ColorPolicy, Duration, Harness, Instant, LifecycleEvent, ProgressSnapshot, TestId,
        TestOutcome, TestStateKind, Vt100, bench_histogram_lines, bench_progress_trailing,
        fake_bench_report, fake_bench_state, fake_snapshot, running_line, strip_ansi, thread,
    };

    /// The post-completion summary block (`emit_completion_block`'s
    /// `detailed_summary` + ascii histogram) is unchanged: progress
    /// events update the live region only, never the completion path.
    #[rudzio::test]
    fn bench_completion_summary_unchanged() -> anyhow::Result<()> {
        let cols = 100_usize;
        let height = 24_usize;
        let harness = Harness::spawn_with_size(Some((cols, height)))?;
        let test_id = TestId::next();
        let drawer_owner_thread = harness.drawer_thread.thread().id();

        harness.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "rudzio::bench_live",
            test_name: "summary",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;
        harness.life_tx.send(LifecycleEvent::BenchProgress {
            test_id,
            snapshot: fake_snapshot(500, 1000),
        })?;
        thread::sleep(Duration::from_millis(80));

        let report = fake_bench_report(40);
        harness.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Benched {
                elapsed: Duration::from_millis(420),
                report,
            },
        })?;
        thread::sleep(Duration::from_millis(80));

        let captured = harness.finish()?;
        // The summary lands AFTER the [BENCH] completion line.
        let stripped = strip_ansi(&captured);
        for needle in ["samples:", "percentiles:", "p50:", "histogram:"] {
            anyhow::ensure!(
                stripped.contains(needle),
                "post-completion summary missing `{needle}`; captured (stripped):\n{stripped}",
            );
        }
        Ok(())
    }

    /// `bench_histogram_lines` paints a row of `▁▂▃▄▅▆▇█` block-drawing
    /// chars when a non-empty histogram is given and there's vertical
    /// room. Each row is clipped to `cols-1` so it can never wrap.
    #[rudzio::test]
    fn bench_histogram_renders_below_running_row() -> anyhow::Result<()> {
        let snap = fake_snapshot(500, 1000);
        let lines = bench_histogram_lines(&snap, ColorPolicy::off(), 120, 8);
        anyhow::ensure!(
            lines.len() == 2,
            "expected 2 histogram rows (bars + axis); got {} rows: {lines:?}",
            lines.len(),
        );
        let bars_line = lines
            .first()
            .ok_or_else(|| anyhow::anyhow!("histogram returned 0 rows"))?;
        let bars_row = strip_ansi(bars_line);
        let block_chars: &[char] = &[
            '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
            '\u{2588}',
        ];
        anyhow::ensure!(
            bars_row.chars().any(|ch| block_chars.contains(&ch)),
            "bars row missing block-drawing chars: {bars_row:?}",
        );
        let axis_line = lines
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("histogram returned <2 rows"))?;
        let axis_row = strip_ansi(axis_line);
        anyhow::ensure!(
            axis_row.contains('\u{2026}')
                || axis_row.contains("...")
                || axis_row.contains('\u{b5}'),
            "axis row should show min … max range: {axis_row:?}",
        );
        // `cols-1` DECAWM rule
        for (idx, line) in lines.iter().enumerate() {
            anyhow::ensure!(
                strip_ansi(line).chars().count() <= 119,
                "histogram row[{idx}] exceeds cols-1=119: {line:?}",
            );
        }
        Ok(())
    }

    /// The trailing block adapts to width, dropping richer info on
    /// narrow terminals so the row never wraps. Specific thresholds:
    /// 100+ → bar+cov, 80+ → bar no cov, 60+ → no bar w/ p50,
    /// 50+ → pct + done/total, <50 → pct only.
    #[rudzio::test]
    fn bench_progress_adaptive_width_drops_components() -> anyhow::Result<()> {
        let snap = fake_snapshot(421, 1000);
        let cases: &[(usize, &[&str], &[&str])] = &[
            (
                200,
                &["[", "\u{2588}", "42%", "421/1000", "p50=", "p95=", "cov="],
                &[],
            ),
            (
                100,
                &["[", "\u{2588}", "42%", "421/1000", "p50=", "p95=", "cov="],
                &[],
            ),
            (
                80,
                &["[", "\u{2588}", "42%", "421/1000", "p50=", "p95="],
                &["cov="],
            ),
            (60, &["42%", "421/1000", "p50="], &["[", "p95=", "cov="]),
            (50, &["42%", "421/1000"], &["[", "p50=", "p95=", "cov="]),
            (40, &["42%"], &["[", "421/1000", "p50=", "p95=", "cov="]),
        ];
        for &(cols, expect_present, expect_absent) in cases {
            let trailing = bench_progress_trailing(&snap, cols, Duration::from_millis(120));
            for needle in expect_present {
                anyhow::ensure!(
                    trailing.contains(needle),
                    "cols={cols}: expected `{needle}` in trailing, got {trailing:?}",
                );
            }
            for needle in expect_absent {
                anyhow::ensure!(
                    !trailing.contains(needle),
                    "cols={cols}: did NOT expect `{needle}` in trailing, got {trailing:?}",
                );
            }
        }
        Ok(())
    }

    /// Trailing block at width ≥100 contains every component the user
    /// wants live: progress bar, percent, `done/total`, p50, p95, cov.
    #[rudzio::test]
    fn bench_progress_appears_in_trailing_block() -> anyhow::Result<()> {
        let snap = fake_snapshot(421, 1000);
        let trailing = bench_progress_trailing(&snap, 200, Duration::from_millis(120));
        for needle in [
            "[", "\u{2588}", "\u{2591}", "]", "42%", "421/1000", "p50=", "p95=", "cov=4.3%",
        ] {
            anyhow::ensure!(
                trailing.contains(needle),
                "trailing block missing `{needle}` at cols=200; got {trailing:?}",
            );
        }
        Ok(())
    }

    /// End-to-end: drive a `Drawer` through a sequence of
    /// `BenchProgress` events, then a `Benched` completion. Replay the
    /// captured byte stream into a Vt100 simulator and assert the
    /// scrollback contains zero `[BENCH]` rows or histogram-bar rows
    /// — they must be cleared in place by the cursor-up clear before
    /// the next paint.
    #[rudzio::test]
    fn bench_progress_leaves_no_stripes_in_scrollback() -> anyhow::Result<()> {
        let cols = 80_usize;
        let height = 12_usize;
        let harness = Harness::spawn_with_size(Some((cols, height)))?;
        let test_id = TestId::next();
        let drawer_owner_thread = harness.drawer_thread.thread().id();

        harness.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "rudzio::bench_live",
            test_name: "stripes",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;

        for done in [10_usize, 100, 250, 500, 750, 999] {
            harness.life_tx.send(LifecycleEvent::BenchProgress {
                test_id,
                snapshot: fake_snapshot(done, 1000),
            })?;
            thread::sleep(Duration::from_millis(60));
        }

        let report = fake_bench_report(20);
        harness.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Benched {
                elapsed: Duration::from_millis(420),
                report,
            },
        })?;
        thread::sleep(Duration::from_millis(80));

        let captured = harness.finish()?;
        let mut term = Vt100::new(cols, height);
        term.feed(captured.as_bytes())?;

        // Live-region characteristic chars: progress bar uses `█░`,
        // histogram uses `▁▂▃▄▅▆▇█`. The post-completion summary's
        // ASCII histogram uses `#` characters only — so a row in
        // scrollback containing any of these block chars is exclusively
        // a live-region paint that escaped the cursor-up clear.
        let live_chars: &[char] = &[
            '\u{2591}', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}',
            '\u{2587}',
        ];
        let stripes: Vec<String> = term
            .scrollback
            .iter()
            .filter(|line| line.chars().any(|ch| live_chars.contains(&ch)))
            .cloned()
            .collect();
        anyhow::ensure!(
            stripes.is_empty(),
            "live-region repaint left {} stale row(s) in scrollback after the bench \
             finished — the cursor-up clear undercounted rows.\nStripes:\n{stripes:#?}\n\
             Full scrollback:\n{:#?}",
            stripes.len(),
            term.scrollback,
        );
        Ok(())
    }

    /// Width sweep: every row painted by `running_line` (with a Bench
    /// state) and `bench_histogram_lines` must fit in `cols` chars,
    /// never `cols+1`. Mirrors the DECAWM defence
    /// `live_region_rows_never_exceed_terminal_width` enforces for
    /// `[RUN]` rows.
    #[rudzio::test]
    fn bench_progress_never_exceeds_terminal_width() -> anyhow::Result<()> {
        // A long qualified name that'd otherwise make the row overrun.
        let mut state = fake_bench_state(fake_snapshot(421, 1000));
        state.module_path = "rudzio::very::deeply::nested::module::path";
        state.test_name = "with_a_very_long_descriptive_test_name_indeed";

        for cols in [40_usize, 60, 80, 100, 120, 200, 400] {
            let row = running_line("tokio::Multithread", &state, ColorPolicy::off(), cols);
            let visible = strip_ansi(&row).chars().count();
            anyhow::ensure!(
                visible <= cols,
                "running_line @ cols={cols}: visible={visible} exceeds cols\n{row:?}",
            );
            let TestStateKind::Bench { snapshot } = &state.kind else {
                anyhow::bail!("test fixture must set TestStateKind::Bench");
            };
            let snap: &ProgressSnapshot = snapshot;
            for (idx, line) in bench_histogram_lines(snap, ColorPolicy::off(), cols, 24)
                .iter()
                .enumerate()
            {
                let visible_hist = strip_ansi(line).chars().count();
                anyhow::ensure!(
                    visible_hist <= cols,
                    "histogram[{idx}] @ cols={cols}: visible={visible_hist} exceeds cols\n{line:?}",
                );
            }
        }
        Ok(())
    }
}

/// Tiny VT100-style emulator. Just enough to interpret what the
/// drawer writes: printing chars, `\n`, `\r`, cursor-up (`ESC [ N A`),
/// erase-in-display (`ESC [ J`), and DECAWM auto-wrap. Other CSIs
/// (SGR colour codes) are consumed and ignored. Anything that scrolls
/// off the top of the viewport lands in `scrollback`.
///
/// Duplicated from `tests/render_idle_redraw.rs` — `cargo test` integration
/// tests don't share modules cleanly, and the simulator is small enough that
/// the duplication beats the cost of refactoring a `tests/common/` module.
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
    /// nominal width.
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
    /// cursor is past `cols`, DECAWM auto-wrap fires before writing.
    fn put_char(&mut self, ch: char) {
        if self.cur_col >= self.cols {
            self.advance_row();
            self.cur_col = 0;
        }
        let Some(row) = self.rows.get_mut(self.cur_row) else {
            return;
        };
        let row_chars = row.chars().count();
        if self.cur_col >= row_chars {
            for _ in row_chars..self.cur_col {
                row.push(' ');
            }
            row.push(ch);
        } else {
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

/// Build a `Report` rich enough that `emit_completion_block` paints
/// `samples:`, `percentiles:`, `p50:`, `histogram:` headers below
/// the post-completion `[BENCH]` row.
fn fake_bench_report(samples: usize) -> Report {
    let pattern: [u64; 7] = [10, 12, 14, 16, 18, 20, 22];
    let durations: Vec<Duration> = pattern
        .iter()
        .copied()
        .cycle()
        .take(samples)
        .map(Duration::from_micros)
        .collect();
    Report::new(
        Vec::new(),
        samples,
        0,
        durations,
        format!("Sequential({samples})"),
        Duration::from_millis(50),
    )
}

/// Build a `TestState` that's already in the `Bench` kind so callers
/// don't have to thread a real benchmark through the framework just
/// to exercise the renderer.
fn fake_bench_state(snapshot: ProgressSnapshot) -> TestState {
    let started_at = Instant::now()
        .checked_sub(Duration::from_millis(120))
        .unwrap_or_else(Instant::now);
    TestState::new(
        TestStateIdent::new(
            "rudzio::bench_live",
            "tokio::Multithread",
            started_at,
            "demo",
            thread::current().id(),
        ),
        TestStateBuffers::empty(),
        TestStateKind::Bench {
            snapshot: Box::new(snapshot),
        },
    )
}

/// Synthetic snapshot with a non-trivial histogram so
/// `bench_histogram_lines` actually paints bars. `cov` is a sane
/// finite value so the wide trailing block exercises the cov branch.
const fn fake_snapshot(done: usize, total: usize) -> ProgressSnapshot {
    let mut histogram = [0_u32; HISTOGRAM_BUCKETS];
    histogram[3] = 4;
    histogram[7] = 12;
    histogram[11] = 28;
    histogram[15] = 40;
    histogram[19] = 28;
    histogram[23] = 12;
    histogram[27] = 4;
    ProgressSnapshot::new(
        done,
        total,
        DistSummary::new(
            Some(43_u16),
            histogram,
            Duration::from_micros(50),
            Duration::from_micros(5),
            Duration::from_micros(12),
            Duration::from_micros(28),
        ),
    )
}

/// Strip ANSI CSI escape sequences (`ESC [ … final-byte`) so we can
/// count the *visible* width of a rendered row.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
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
    temp_dir().join(format!("rudzio-bench-live-test-{pid}-{nanos}-{seq}.log"))
}
