//! Benchmarking instrument.
//!
//! A test annotated with `#[rudzio::test(benchmark = <strategy>)]` runs the
//! body multiple times under the given [`Strategy`] when the runner is
//! invoked with `--bench`. Without `--bench`, the body runs exactly once as
//! a smoke test — the bench annotation is a no-op, so every bench test is
//! also a valid regular test without changing anything.
//!
//! The strategy interface is a single [`Strategy::run`] method that takes a
//! closure producing a fresh future per call and returns a [`BenchReport`]
//! aggregating per-iteration timings plus failure and panic counts. Two
//! primitive strategies ship with rudzio: [`strategy::Sequential`] (N
//! one-after-another iterations) and [`strategy::Concurrent`] (N
//! `join_all`-driven concurrent futures on the same task). Custom
//! strategies can be written by implementing [`Strategy`] directly — the
//! trait is intentionally minimal so composition (run A then B, repeat K
//! rounds, etc.) is just a matter of writing a new impl.

pub mod strategy;

use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

use crate::test_case::BoxError;

/// Number of linear histogram buckets carried in a
/// [`BenchProgressSnapshot`].
pub const HISTOGRAM_BUCKETS: usize = 32;

/// Cheap, fixed-size summary of a benchmark's progress.
///
/// Emitted from [`Strategy::run`] roughly every 1% of iterations and
/// consumed by the live-region renderer to draw a progress bar, p50 /
/// p95 / cov, and a mini-histogram below the running row.
///
/// `Copy` so it travels through the lifecycle channel without
/// allocation. The histogram is pre-binned (linear over `[min, max]`)
/// so the drawer doesn't need to keep the raw per-iteration sample
/// vector around.
#[derive(Debug, Clone, Copy)]
pub struct BenchProgressSnapshot {
    /// Coefficient of variation (σ / mean) of the successful samples.
    /// `f32::NAN` when fewer than two samples are available; renderers
    /// must guard with `is_finite()`.
    pub cov: f32,
    /// Iterations completed so far (success + failure + panic).
    pub done: usize,
    /// Pre-binned histogram: 32 linear buckets over `[min, max]`.
    /// All zero when no successful samples exist yet.
    pub histogram: [u32; HISTOGRAM_BUCKETS],
    /// Largest successful sample, or `Duration::ZERO` when empty.
    pub max: Duration,
    /// Smallest successful sample, or `Duration::ZERO` when empty.
    pub min: Duration,
    /// Median of the successful samples seen so far. `Duration::ZERO`
    /// when no successful samples exist yet.
    pub p50: Duration,
    /// 95th percentile of the successful samples. `Duration::ZERO`
    /// when no successful samples exist yet.
    pub p95: Duration,
    /// Total iterations the strategy intends to run.
    pub total: usize,
}

/// Per-iteration results gathered by a [`Strategy`] run.
///
/// `samples` holds the elapsed time of every iteration that completed
/// successfully (returned `Ok(())`); `failures` holds the rendered error
/// string from every iteration that returned `Err(_)`; `panics` counts
/// iterations whose future panicked mid-poll.
///
/// `total_elapsed` is the wall-clock time between the strategy starting
/// and the last iteration finishing — not the sum of sample durations
/// (those overlap for concurrent strategies).
#[derive(Debug, Clone)]
pub struct BenchReport {
    /// Error strings from iterations that returned `Err(_)`.
    pub failures: Vec<String>,
    /// Total number of iterations the strategy attempted.
    pub iterations: usize,
    /// Count of iterations whose future panicked mid-poll.
    pub panics: usize,
    /// Elapsed time of every iteration that returned `Ok(())`.
    pub samples: Vec<Duration>,
    /// Human-readable strategy label, e.g. `"Sequential(1000)"`.
    pub strategy: String,
    /// Wall-clock time the whole strategy run took.
    pub total_elapsed: Duration,
}

/// Composable benchmark strategy.
///
/// A strategy decides how many times, and with what concurrency, to call
/// the test body. `body` is a closure that produces a fresh future per
/// call; the strategy invokes it repeatedly and aggregates per-iteration
/// timings into a [`BenchReport`].
///
/// The trait is deliberately minimal: writing a new strategy is just a
/// new `impl`. Composition (warm-up then measure, repeat K rounds,
/// sequence A-then-B) is a matter of wrapping one or more inner
/// strategies in a new type and delegating. No runtime registry, no
/// magic — whatever the user writes at `benchmark = <expr>` is the
/// concrete type the macro-generated code calls `.run(...)` on.
pub trait Strategy {
    /// Run the body according to this strategy, collecting per-iteration
    /// timings into a [`BenchReport`].
    ///
    /// `body` is called afresh for every iteration — the future it
    /// returns is polled to completion (or panic) inside a
    /// [`std::panic::catch_unwind`] boundary so one bad iteration
    /// doesn't abort the whole bench.
    ///
    /// `on_progress` is invoked at strategy entry (with a zero-progress
    /// placeholder so the live-region renderer can flip the row tag
    /// from `[RUN]` to `[BENCH]` immediately) and roughly every 1% of
    /// iterations thereafter, with the latest [`BenchProgressSnapshot`].
    /// Implementations that omit progress should still call it once
    /// at entry — a `|_| ()` no-op closure is acceptable from callers
    /// that don't care.
    fn run<B, Fut, P>(&self, body: B, on_progress: P) -> impl Future<Output = BenchReport>
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
        P: FnMut(BenchProgressSnapshot);
}

impl BenchProgressSnapshot {
    /// Build a snapshot by cloning + sorting `samples` and binning
    /// them into `HISTOGRAM_BUCKETS` linear buckets over `[min, max]`.
    ///
    /// Cost is `O(n log n)`. Strategies call this at most ~100 times
    /// per run (capped by their stride), so total amortised cost is
    /// bounded even for high iteration counts.
    #[must_use]
    #[inline]
    pub fn from_samples(samples: &[Duration], done: usize, total: usize) -> Self {
        if samples.is_empty() {
            let mut snap = Self::initial(total);
            snap.done = done;
            return snap;
        }
        let mut sorted: Vec<Duration> = samples.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        let min = sorted[0];
        let max = sorted[n.saturating_sub(1)];

        // Nearest-rank percentile, matching `BenchReport::percentile`.
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "Benchmark rank approximation; absolute precision not required."
        )]
        let rank = |percentile: f64| -> usize {
            ((percentile * n as f64).ceil() as usize)
                .saturating_sub(1)
                .min(n.saturating_sub(1))
        };
        let p50 = sorted[rank(0.50)];
        let p95 = sorted[rank(0.95)];

        // Coefficient of variation: σ / mean. NaN when n<2 or mean=0.
        let cov = if n < 2 {
            f32::NAN
        } else {
            let total_nanos: u128 = sorted.iter().map(Duration::as_nanos).sum();
            #[expect(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                reason = "Benchmark statistics; precision loss well within measurement noise."
            )]
            {
                let mean = total_nanos as f64 / n as f64;
                if mean == 0.0_f64 {
                    f32::NAN
                } else {
                    let variance = sorted
                        .iter()
                        .map(|sample| {
                            let diff = sample.as_nanos() as f64 - mean;
                            diff * diff
                        })
                        .sum::<f64>()
                        / n as f64;
                    (variance.sqrt() / mean) as f32
                }
            }
        };

        // Linear binning over [min, max] into HISTOGRAM_BUCKETS bins.
        let mut histogram = [0_u32; HISTOGRAM_BUCKETS];
        let min_ns = min.as_nanos();
        let max_ns = max.as_nanos();
        let span = max_ns.saturating_sub(min_ns).max(1);
        let bucket_span = span.div_ceil(HISTOGRAM_BUCKETS as u128).max(1);
        for sample in &sorted {
            let offset = sample.as_nanos().saturating_sub(min_ns);
            let idx = ((offset / bucket_span) as usize).min(HISTOGRAM_BUCKETS.saturating_sub(1));
            histogram[idx] = histogram[idx].saturating_add(1);
        }

        Self {
            cov,
            done,
            histogram,
            max,
            min,
            p50,
            p95,
            total,
        }
    }

    /// Zero-progress placeholder emitted at iteration 0 so the
    /// renderer flips the running-row tag from `[RUN]` to `[BENCH]`
    /// immediately on strategy entry — before any samples have
    /// accumulated.
    #[must_use]
    #[inline]
    pub const fn initial(total: usize) -> Self {
        Self {
            done: 0,
            total,
            p50: Duration::ZERO,
            p95: Duration::ZERO,
            min: Duration::ZERO,
            max: Duration::ZERO,
            cov: f32::NAN,
            histogram: [0_u32; HISTOGRAM_BUCKETS],
        }
    }
}

impl BenchReport {
    /// Render a horizontal ASCII histogram with `buckets` bars of `width`
    /// characters each.
    ///
    /// Returns an empty string when there are no successful samples; the
    /// range is `[min, max]` split into equal-width linear buckets. Each
    /// line is `"  [lo..hi) |######  count"`.
    #[must_use]
    #[inline]
    pub fn ascii_histogram(&self, buckets: usize, width: usize) -> String {
        if self.samples.is_empty() || buckets == 0 {
            return String::new();
        }
        let min_ns = self.min().unwrap_or_default().as_nanos();
        let max_ns = self.max().unwrap_or_default().as_nanos();
        let span = max_ns.saturating_sub(min_ns).max(1);
        let bucket_span = span.div_ceil(buckets as u128).max(1);

        let mut counts = vec![0_usize; buckets];
        for sample in &self.samples {
            let offset = sample.as_nanos().saturating_sub(min_ns);
            let idx = ((offset / bucket_span) as usize).min(buckets.saturating_sub(1));
            counts[idx] = counts[idx].saturating_add(1);
        }
        let max_count = counts.iter().copied().max().unwrap_or(1).max(1);

        let mut out = String::new();
        for (i, count) in counts.iter().enumerate() {
            let lo = Duration::from_nanos(
                u64::try_from(min_ns + (i as u128) * bucket_span).unwrap_or(u64::MAX),
            );
            let hi = Duration::from_nanos(
                u64::try_from(min_ns + ((i + 1) as u128) * bucket_span).unwrap_or(u64::MAX),
            );
            let bar_len = (count * width) / max_count;
            let bar = "#".repeat(bar_len);
            let _write_ret: Result<(), fmt::Error> = writeln!(
                out,
                "  [{lo:>9.2?} .. {hi:>9.2?}) |{bar:<width$}  {count}"
            );
        }
        out
    }

    /// Coefficient of variation: σ / mean. A unitless measure of
    /// relative spread (1.0 = σ equals the mean — very noisy). `None`
    /// when mean is zero or σ is unavailable.
    #[must_use]
    #[inline]
    pub fn coefficient_of_variation(&self) -> Option<f64> {
        let mean = self.mean()?.as_nanos();
        let sd = self.std_dev()?.as_nanos();
        if mean == 0 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "Benchmark statistics; precision loss well within measurement noise."
        )]
        {
            Some(sd as f64 / mean as f64)
        }
    }

    /// Multi-line detailed statistics block — intended for rendering
    /// directly after a benchmark status line. Lines are indented two
    /// spaces so they visually nest under the `[BENCH]` status row.
    ///
    /// Includes: sample count, throughput, wall-clock elapsed, min /
    /// max / range, mean / median, σ / MAD / coefficient of variation,
    /// p1 / p5 / p10 / p25 / p50 / p75 / p90 / p95 / p99 / p99.9, IQR,
    /// outlier count (>3σ), and failure / panic tallies when present.
    #[must_use]
    #[inline]
    pub fn detailed_summary(&self) -> String {
        let n = self.samples.len();
        if n == 0 {
            let mut out = format!(
                "  no successful samples (iterations: {})\n",
                self.iterations
            );
            if !self.failures.is_empty() {
                let _early_failures_ret: Result<(), fmt::Error> =
                    writeln!(out, "  failed iterations: {}", self.failures.len());
            }
            if self.panics > 0 {
                let _early_panics_ret: Result<(), fmt::Error> =
                    writeln!(out, "  panicked iterations: {}", self.panics);
            }
            return out;
        }
        let mut out = String::new();
        let _samples_ret: Result<(), fmt::Error> =
            writeln!(out, "  samples:           {n}");
        let _wallclock_ret: Result<(), fmt::Error> =
            writeln!(out, "  wall-clock:        {:.2?}", self.total_elapsed);
        if let Some(throughput) = self.throughput_per_sec() {
            let _throughput_ret: Result<(), fmt::Error> =
                writeln!(out, "  throughput:        {throughput:.2} iter/s");
        }
        if let (Some(min), Some(max)) = (self.min(), self.max()) {
            let _minmax_ret: Result<(), fmt::Error> =
                writeln!(out, "  min / max:         {min:.2?} / {max:.2?}");
        }
        if let Some(range) = self.range() {
            let _range_ret: Result<(), fmt::Error> =
                writeln!(out, "  range:             {range:.2?}");
        }
        if let Some(mean) = self.mean() {
            let _mean_ret: Result<(), fmt::Error> =
                writeln!(out, "  mean:              {mean:.2?}");
        }
        if let Some(median) = self.median() {
            let _median_ret: Result<(), fmt::Error> =
                writeln!(out, "  median:            {median:.2?}");
        }
        if let Some(sd) = self.std_dev() {
            let _sd_ret: Result<(), fmt::Error> =
                writeln!(out, "  std dev:           {sd:.2?}");
        }
        if let Some(mad) = self.mad() {
            let _mad_ret: Result<(), fmt::Error> =
                writeln!(out, "  MAD:               {mad:.2?}");
        }
        if let Some(cv) = self.coefficient_of_variation() {
            let _cv_ret: Result<(), fmt::Error> =
                writeln!(out, "  coeff of variation:{cv:>8.3}");
        }
        if let Some(iqr) = self.iqr() {
            let _iqr_ret: Result<(), fmt::Error> =
                writeln!(out, "  IQR (p75 − p25):   {iqr:.2?}");
        }
        if let Some(outliers) = self.outlier_count(3.0) {
            let _outliers_ret: Result<(), fmt::Error> =
                writeln!(out, "  outliers (>3σ):    {outliers}");
        }
        out.push_str("  percentiles:\n");
        for (percentile, label) in [
            (0.01, "p1"),
            (0.05, "p5"),
            (0.10, "p10"),
            (0.25, "p25"),
            (0.50, "p50"),
            (0.75, "p75"),
            (0.90, "p90"),
            (0.95, "p95"),
            (0.99, "p99"),
            (0.999, "p99.9"),
        ] {
            if let Some(value) = self.percentile(percentile) {
                let _percentile_ret: Result<(), fmt::Error> =
                    writeln!(out, "    {label:>6}:         {value:.2?}");
            }
        }
        if !self.failures.is_empty() {
            let _failures_ret: Result<(), fmt::Error> =
                writeln!(out, "  failed iterations: {}", self.failures.len());
        }
        if self.panics > 0 {
            let _panics_ret: Result<(), fmt::Error> =
                writeln!(out, "  panicked iterations: {}", self.panics);
        }
        out
    }

    /// Interquartile range: p75 - p25. `None` when percentile
    /// computation yields nothing (no samples).
    #[must_use]
    #[inline]
    pub fn iqr(&self) -> Option<Duration> {
        let p25 = self.percentile(0.25)?;
        let p75 = self.percentile(0.75)?;
        Some(p75.saturating_sub(p25))
    }

    /// `true` when every iteration completed without errors or panics.
    #[inline]
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.failures.is_empty() && self.panics == 0
    }

    /// Median absolute deviation — a robust dispersion measure that
    /// is less sensitive to outliers than the standard deviation.
    /// `None` when there are no samples.
    #[must_use]
    #[inline]
    pub fn mad(&self) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let median_ns = self.median()?.as_nanos();
        let mut deviations: Vec<u128> = self
            .samples
            .iter()
            .map(|sample| sample.as_nanos().abs_diff(median_ns))
            .collect();
        deviations.sort_unstable();
        let mid = deviations.len() / 2;
        Some(Duration::from_nanos(
            u64::try_from(deviations[mid]).unwrap_or(u64::MAX),
        ))
    }

    /// Largest successful-iteration duration, or `None` when there are no
    /// successful samples.
    #[must_use]
    #[inline]
    pub fn max(&self) -> Option<Duration> {
        self.samples.iter().copied().max()
    }

    /// Arithmetic mean of successful-iteration durations, or `None` when
    /// there are no successful samples.
    #[must_use]
    #[inline]
    pub fn mean(&self) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let total_nanos: u128 = self.samples.iter().map(Duration::as_nanos).sum();
        let mean_nanos = total_nanos / (self.samples.len() as u128);
        Some(Duration::from_nanos(
            u64::try_from(mean_nanos).unwrap_or(u64::MAX),
        ))
    }

    /// Sample at the median (50th percentile), or `None` when empty.
    #[inline]
    #[must_use]
    pub fn median(&self) -> Option<Duration> {
        self.percentile(0.5)
    }

    /// Smallest successful-iteration duration, or `None` if every iteration
    /// failed or the strategy attempted zero iterations.
    #[must_use]
    #[inline]
    pub fn min(&self) -> Option<Duration> {
        self.samples.iter().copied().min()
    }

    /// Rough outlier count — samples more than `k × σ` from the mean
    /// (default `k = 3`). `None` when σ is unavailable.
    #[must_use]
    #[inline]
    pub fn outlier_count(&self, sigma_multiplier: f64) -> Option<usize> {
        let mean_ns = self.mean()?.as_nanos();
        let sd_ns = self.std_dev()?.as_nanos();
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "Benchmark statistics; precision loss well within measurement noise."
        )]
        {
            let threshold = (sd_ns as f64 * sigma_multiplier) as u128;
            Some(
                self.samples
                    .iter()
                    .filter(|sample| sample.as_nanos().abs_diff(mean_ns) > threshold)
                    .count(),
            )
        }
    }

    /// Sample at the `p`-th percentile (`0.0..=1.0`, nearest-rank) or
    /// `None` when there are no successful samples.
    ///
    /// `percentile(0.5)` is the median; `percentile(0.99)` is the p99.
    /// Returns `None` when `p` is outside `[0.0, 1.0]`.
    #[must_use]
    #[inline]
    pub fn percentile(&self, percentile: f64) -> Option<Duration> {
        if !(0.0..=1.0).contains(&percentile) || self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        // Nearest-rank definition: index = ceil(p * N) - 1, clamped.
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "Benchmark rank approximation; absolute precision not required."
        )]
        let rank = ((percentile * sorted.len() as f64).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len().saturating_sub(1));
        Some(sorted[rank])
    }

    /// Range: max - min. `None` when there are no samples.
    #[must_use]
    #[inline]
    pub fn range(&self) -> Option<Duration> {
        Some(self.max()?.saturating_sub(self.min()?))
    }

    /// Population standard deviation of the successful-iteration
    /// durations. `None` when fewer than two samples are available
    /// (σ is undefined for n ≤ 1).
    #[must_use]
    #[inline]
    pub fn std_dev(&self) -> Option<Duration> {
        if self.samples.len() < 2 {
            return None;
        }
        let mean_ns = self.mean()?.as_nanos();
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "Benchmark statistics; precision loss well within measurement noise."
        )]
        {
            let n = self.samples.len() as f64;
            let mean = mean_ns as f64;
            let variance: f64 = self
                .samples
                .iter()
                .map(|sample| {
                    let diff = sample.as_nanos() as f64 - mean;
                    diff * diff
                })
                .sum::<f64>()
                / n;
            Some(Duration::from_nanos(variance.sqrt() as u64))
        }
    }

    /// A single-line summary: `"min X, p50 Y, p95 Z, max W (N samples)"`.
    #[must_use]
    #[inline]
    pub fn summary_line(&self) -> String {
        let n = self.samples.len();
        if n == 0 {
            return format!("no successful samples (iterations={})", self.iterations);
        }
        format!(
            "min {:.2?}, p50 {:.2?}, p95 {:.2?}, max {:.2?} ({n} samples)",
            self.min().unwrap_or_default(),
            self.median().unwrap_or_default(),
            self.percentile(0.95).unwrap_or_default(),
            self.max().unwrap_or_default(),
        )
    }

    /// Throughput in successful iterations per second, derived from
    /// sample count and [`Self::total_elapsed`]. For
    /// [`strategy::Concurrent`] this reflects *real* throughput
    /// (wall-clock) rather than per-iteration latency, which matters
    /// when comparing strategies.
    ///
    /// [`strategy::Concurrent`]: crate::bench::strategy::Concurrent
    #[must_use]
    #[inline]
    pub fn throughput_per_sec(&self) -> Option<f64> {
        let secs = self.total_elapsed.as_secs_f64();
        if secs <= 0.0 || self.samples.is_empty() {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "Benchmark statistics; precision loss well within measurement noise."
        )]
        {
            Some(self.samples.len() as f64 / secs)
        }
    }
}
