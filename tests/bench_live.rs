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

use std::fs::{File, OpenOptions};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Sender, bounded, unbounded};

use rudzio::bench::{ProgressSnapshot, Report, HISTOGRAM_BUCKETS};
use rudzio::config::{Format, OutputMode};
use rudzio::output::color::Policy as ColorPolicy;
use rudzio::output::events::{LifecycleEvent, PipeChunk, TestId, TestState, TestStateKind};
use rudzio::output::render::{
    Drawer, bench_histogram_lines, bench_progress_trailing, running_line, spawn_drawer,
};
use rudzio::suite::TestOutcome;

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
        rudzio::bench::DistSummary::new(
            Some(43_u16),
            histogram,
            Duration::from_micros(50),
            Duration::from_micros(5),
            Duration::from_micros(12),
            Duration::from_micros(28),
        ),
    )
}

fn fake_bench_state(snapshot: ProgressSnapshot) -> TestState {
    use rudzio::output::events::{TestStateBuffers, TestStateIdent};
    TestState::new(
        TestStateIdent::new(
            "rudzio::bench_live",
            "tokio::Multithread",
            Instant::now().checked_sub(Duration::from_millis(120)).unwrap(),
            "demo",
            thread::current().id(),
        ),
        TestStateBuffers::empty(),
        TestStateKind::Bench { snapshot: Box::new(snapshot) },
    )
}

/// Build a `Report` rich enough that
/// `emit_completion_block` paints `samples:`, `percentiles:`, `p50:`,
/// `histogram:` headers below the post-completion `[BENCH]` row.
fn fake_bench_report(samples: usize) -> Report {
    let mut s: Vec<Duration> = Vec::with_capacity(samples);
    for i in 0..samples {
        s.push(Duration::from_micros((10 + (i % 7) * 2) as u64));
    }
    Report::new(
        Vec::new(),
        samples,
        0,
        s,
        format!("Sequential({samples})"),
        Duration::from_millis(50),
    )
}

struct Harness {
    path: PathBuf,
    reader: File,
    life_tx: Sender<LifecycleEvent>,
    pipe_tx: Sender<PipeChunk>,
    shutdown_tx: Sender<()>,
    drawer_thread: thread::JoinHandle<()>,
}

impl Harness {
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
        ColorPolicy, Duration, Harness, Instant, LifecycleEvent, ProgressSnapshot, TestId,
        TestOutcome, TestStateKind, Vt100, bench_histogram_lines, bench_progress_trailing,
        fake_bench_report, fake_bench_state, fake_snapshot, running_line, strip_ansi, thread,
    };

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
        let bars_row = strip_ansi(&lines[0]);
        let block_chars: &[char] = &['\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
        anyhow::ensure!(
            bars_row.chars().any(|c| block_chars.contains(&c)),
            "bars row missing block-drawing chars: {bars_row:?}",
        );
        let axis_row = strip_ansi(&lines[1]);
        anyhow::ensure!(
            axis_row.contains('\u{2026}') || axis_row.contains("...") || axis_row.contains('\u{b5}'),
            "axis row should show min … max range: {axis_row:?}",
        );
        // `cols-1` DECAWM rule
        for (i, line) in lines.iter().enumerate() {
            anyhow::ensure!(
                strip_ansi(line).chars().count() <= 119,
                "histogram row[{i}] exceeds cols-1=119: {line:?}",
            );
        }
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
            let snap: &ProgressSnapshot = match &state.kind {
                TestStateKind::Bench { snapshot } => snapshot,
                TestStateKind::Running => unreachable!("test fixture sets Bench state"),
                _ => unreachable!("test fixture sets Bench state"),
            };
            for (i, line) in bench_histogram_lines(snap, ColorPolicy::off(), cols, 24)
                .iter()
                .enumerate()
            {
                let visible = strip_ansi(line).chars().count();
                anyhow::ensure!(
                    visible <= cols,
                    "histogram[{i}] @ cols={cols}: visible={visible} exceeds cols\n{line:?}",
                );
            }
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
        let h = Harness::spawn_with_size(Some((cols, height)))?;
        let test_id = TestId::next();
        let drawer_owner_thread = h.drawer_thread.thread().id();

        h.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "rudzio::bench_live",
            test_name: "stripes",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;

        for done in [10, 100, 250, 500, 750, 999] {
            h.life_tx.send(LifecycleEvent::BenchProgress {
                test_id,
                snapshot: fake_snapshot(done, 1000),
            })?;
            thread::sleep(Duration::from_millis(60));
        }

        let report = fake_bench_report(20);
        h.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Benched {
                elapsed: Duration::from_millis(420),
                report,
            },
        })?;
        thread::sleep(Duration::from_millis(80));

        let captured = h.finish()?;
        let mut term = Vt100::new(cols, height);
        term.feed(captured.as_bytes());

        // Live-region characteristic chars: progress bar uses `█░`,
        // histogram uses `▁▂▃▄▅▆▇█`. The post-completion summary's
        // ASCII histogram uses `#` characters only — so a row in
        // scrollback containing any of these block chars is exclusively
        // a live-region paint that escaped the cursor-up clear.
        let live_chars: &[char] = &['\u{2591}', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}'];
        let stripes: Vec<String> = term
            .scrollback
            .iter()
            .filter(|line| line.chars().any(|c| live_chars.contains(&c)))
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

    /// The post-completion summary block (`emit_completion_block`'s
    /// `detailed_summary` + ascii histogram) is unchanged: progress
    /// events update the live region only, never the completion path.
    #[rudzio::test]
    fn bench_completion_summary_unchanged() -> anyhow::Result<()> {
        let cols = 100_usize;
        let height = 24_usize;
        let h = Harness::spawn_with_size(Some((cols, height)))?;
        let test_id = TestId::next();
        let drawer_owner_thread = h.drawer_thread.thread().id();

        h.life_tx.send(LifecycleEvent::TestStarted {
            test_id,
            module_path: "rudzio::bench_live",
            test_name: "summary",
            runtime_name: "tokio::Multithread",
            thread: drawer_owner_thread,
            at: Instant::now(),
        })?;
        h.life_tx.send(LifecycleEvent::BenchProgress {
            test_id,
            snapshot: fake_snapshot(500, 1000),
        })?;
        thread::sleep(Duration::from_millis(80));

        let report = fake_bench_report(40);
        h.life_tx.send(LifecycleEvent::TestCompleted {
            test_id,
            outcome: TestOutcome::Benched {
                elapsed: Duration::from_millis(420),
                report,
            },
        })?;
        thread::sleep(Duration::from_millis(80));

        let captured = h.finish()?;
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
            self.advance_row();
            self.cur_col = 0;
        }
        let row = &mut self.rows[self.cur_row];
        let row_chars = row.chars().count();
        if self.cur_col >= row_chars {
            for _ in row_chars..self.cur_col {
                row.push(' ');
            }
            row.push(c);
        } else {
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
            _ => {}
        }
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
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
        .map_or(0, |d| d.as_nanos());
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rudzio-bench-live-test-{pid}-{nanos}-{n}.log"))
}
