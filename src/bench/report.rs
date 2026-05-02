//! Per-iteration results gathered by a [`crate::bench::Strategy`] run.
//!
//! `samples` holds the elapsed time of every iteration that completed
//! successfully (returned `Ok(())`); `failures` holds the rendered error
//! string from every iteration that returned `Err(_)`; `panics` counts
//! iterations whose future panicked mid-poll.
//!
//! `total_elapsed` is the wall-clock time between the strategy starting
//! and the last iteration finishing — not the sum of sample durations
//! (those overlap for concurrent strategies).

use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

use crate::common::time::fmt_duration;

/// Per-iteration results gathered by a [`crate::bench::Strategy`] run.
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
#[non_exhaustive]
pub struct Report {
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

impl Report {
    /// Append the `percentiles:` block (heading + one line per
    /// percentile that has a value) to `out`. Extracted from
    /// [`Self::detailed_summary`] to keep that fn under
    /// `clippy::too_many_lines`.
    #[inline]
    fn append_percentiles_block(&self, out: &mut String) {
        out.push_str("  percentiles:\n");
        for (permille, label) in [
            (10_u32, "p1"),
            (50_u32, "p5"),
            (100_u32, "p10"),
            (250_u32, "p25"),
            (500_u32, "p50"),
            (750_u32, "p75"),
            (900_u32, "p90"),
            (950_u32, "p95"),
            (990_u32, "p99"),
            (999_u32, "p99.9"),
        ] {
            if let Some(value) = self.percentile_permille(permille) {
                let value_text = fmt_duration(value);
                let _percentile_ret: Result<(), fmt::Error> =
                    writeln!(out, "    {label:>6}:         {value_text}");
            }
        }
    }

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
        let buckets_u128 = u128::try_from(buckets).unwrap_or(u128::MAX);
        let bucket_span = span.div_ceil(buckets_u128).max(1);

        let mut counts = vec![0_usize; buckets];
        for sample in &self.samples {
            let offset = sample.as_nanos().saturating_sub(min_ns);
            let idx = usize::try_from(offset.checked_div(bucket_span).unwrap_or(0))
                .unwrap_or(usize::MAX)
                .min(buckets.saturating_sub(1));
            if let Some(slot) = counts.get_mut(idx) {
                *slot = slot.saturating_add(1);
            }
        }
        let max_count = counts.iter().copied().max().unwrap_or(1).max(1);

        let mut out = String::new();
        for (i, count) in counts.iter().enumerate() {
            let lo_idx = u128::try_from(i).unwrap_or(u128::MAX);
            let hi_idx = u128::try_from(i.saturating_add(1)).unwrap_or(u128::MAX);
            let lo = Duration::from_nanos(
                u64::try_from(min_ns.saturating_add(lo_idx.saturating_mul(bucket_span)))
                    .unwrap_or(u64::MAX),
            );
            let hi = Duration::from_nanos(
                u64::try_from(min_ns.saturating_add(hi_idx.saturating_mul(bucket_span)))
                    .unwrap_or(u64::MAX),
            );
            let bar_len = count
                .saturating_mul(width)
                .checked_div(max_count)
                .unwrap_or(0);
            let bar = "#".repeat(bar_len);
            let lo_text = fmt_duration(lo);
            let hi_text = fmt_duration(hi);
            let _write_ret: Result<(), fmt::Error> = writeln!(
                out,
                "  [{lo_text:>9} .. {hi_text:>9}) |{bar:<width$}  {count}"
            );
        }
        out
    }

    /// Coefficient of variation: σ / mean. A unitless measure of
    /// relative spread (1.0 = σ equals the mean — very noisy). `None`
    /// when mean is zero or σ is unavailable.
    ///
    /// Returned as `cov × 10⁶` packed into a u32 — keeps the math
    /// purely integer (no `float_arithmetic` lint exposure) while
    /// preserving 1e-6 absolute precision. Callers that want a
    /// floating-point view can scale at the print boundary; callers
    /// that just want to format `xx.xxx` can split into integer +
    /// fractional parts in u32 arithmetic.
    #[must_use]
    #[inline]
    pub fn coefficient_of_variation_micro(&self) -> Option<u32> {
        let mean = self.mean()?.as_nanos();
        let sd = self.std_dev()?.as_nanos();
        if mean == 0_u128 {
            return None;
        }
        let cov_micro_u128 = sd
            .saturating_mul(1_000_000_u128)
            .checked_div(mean)
            .unwrap_or(0_u128);
        Some(u32::try_from(cov_micro_u128).unwrap_or(u32::MAX))
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
        let _samples_ret: Result<(), fmt::Error> = writeln!(out, "  samples:           {n}");
        let _wallclock_ret: Result<(), fmt::Error> = writeln!(
            out,
            "  wall-clock:        {}",
            fmt_duration(self.total_elapsed)
        );
        if let Some(throughput_milli) = self.throughput_milli_per_sec() {
            // throughput_milli = iter/s × 1000; render `xxxx.yy iter/s`
            // by splitting into integer (÷1000) and centi-fractional
            // (÷10 % 100) parts — matches the previous `{throughput:.2}`
            // formatting without going through f64.
            let int_part = throughput_milli.checked_div(1_000_u32).unwrap_or(0);
            let centi_part = throughput_milli
                .checked_div(10_u32)
                .unwrap_or(0)
                .checked_rem(100_u32)
                .unwrap_or(0);
            let _throughput_ret: Result<(), fmt::Error> = writeln!(
                out,
                "  throughput:        {int_part}.{centi_part:02} iter/s"
            );
        }
        if let (Some(min), Some(max)) = (self.min(), self.max()) {
            let min_text = fmt_duration(min);
            let max_text = fmt_duration(max);
            let _minmax_ret: Result<(), fmt::Error> =
                writeln!(out, "  min / max:         {min_text} / {max_text}");
        }
        if let Some(range) = self.range() {
            let range_text = fmt_duration(range);
            let _range_ret: Result<(), fmt::Error> =
                writeln!(out, "  range:             {range_text}");
        }
        if let Some(mean) = self.mean() {
            let mean_text = fmt_duration(mean);
            let _mean_ret: Result<(), fmt::Error> =
                writeln!(out, "  mean:              {mean_text}");
        }
        if let Some(median) = self.median() {
            let median_text = fmt_duration(median);
            let _median_ret: Result<(), fmt::Error> =
                writeln!(out, "  median:            {median_text}");
        }
        if let Some(sd) = self.std_dev() {
            let sd_text = fmt_duration(sd);
            let _sd_ret: Result<(), fmt::Error> = writeln!(out, "  std dev:           {sd_text}");
        }
        if let Some(mad) = self.mad() {
            let mad_text = fmt_duration(mad);
            let _mad_ret: Result<(), fmt::Error> = writeln!(out, "  MAD:               {mad_text}");
        }
        if let Some(cv_micro) = self.coefficient_of_variation_micro() {
            // cv_micro carries cov × 10⁶; we render `xxxx.yyy` (3 decimals)
            // by splitting into integer (÷10⁶) and milli-fractional
            // (÷10³ % 10³) parts — purely integer math, no f64.
            let cv_int = cv_micro.checked_div(1_000_000_u32).unwrap_or(0);
            let cv_milli_part = cv_micro
                .checked_div(1_000_u32)
                .unwrap_or(0)
                .checked_rem(1_000_u32)
                .unwrap_or(0);
            let cv_text = format!("{cv_int}.{cv_milli_part:03}");
            let _cv_ret: Result<(), fmt::Error> =
                writeln!(out, "  coeff of variation:{cv_text:>8}");
        }
        if let Some(iqr) = self.iqr() {
            let iqr_text = fmt_duration(iqr);
            let _iqr_ret: Result<(), fmt::Error> = writeln!(out, "  IQR (p75 − p25):   {iqr_text}");
        }
        if let Some(outliers) = self.outlier_count(3_u32) {
            let _outliers_ret: Result<(), fmt::Error> =
                writeln!(out, "  outliers (>3σ):    {outliers}");
        }
        self.append_percentiles_block(&mut out);
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
        let p25 = self.percentile_permille(250_u32)?;
        let p75 = self.percentile_permille(750_u32)?;
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
        let mid = deviations.len().checked_div(2).unwrap_or(0);
        let mid_value = deviations.get(mid).copied().unwrap_or(0_u128);
        Some(Duration::from_nanos(
            u64::try_from(mid_value).unwrap_or(u64::MAX),
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
        let n_u128 = u128::try_from(self.samples.len()).unwrap_or(u128::MAX);
        let mean_nanos = total_nanos.checked_div(n_u128).unwrap_or(0);
        Some(Duration::from_nanos(
            u64::try_from(mean_nanos).unwrap_or(u64::MAX),
        ))
    }

    /// Sample at the median (50th percentile), or `None` when empty.
    #[inline]
    #[must_use]
    pub fn median(&self) -> Option<Duration> {
        self.percentile_permille(500_u32)
    }

    /// Smallest successful-iteration duration, or `None` if every iteration
    /// failed or the strategy attempted zero iterations.
    #[must_use]
    #[inline]
    pub fn min(&self) -> Option<Duration> {
        self.samples.iter().copied().min()
    }

    /// Construct a `Report` from its components.
    #[inline]
    #[must_use]
    pub const fn new(
        failures: Vec<String>,
        iterations: usize,
        panics: usize,
        samples: Vec<Duration>,
        strategy: String,
        total_elapsed: Duration,
    ) -> Self {
        Self {
            failures,
            iterations,
            panics,
            samples,
            strategy,
            total_elapsed,
        }
    }

    /// Rough outlier count — samples more than `k × σ` from the mean
    /// (default `k = 3`). `None` when σ is unavailable.
    ///
    /// Takes an integer multiplier so the threshold computation stays
    /// in u128 domain — no `as` conversions required.
    #[must_use]
    #[inline]
    pub fn outlier_count(&self, sigma_multiplier: u32) -> Option<usize> {
        let mean_ns = self.mean()?.as_nanos();
        let sd_ns = self.std_dev()?.as_nanos();
        let threshold = sd_ns.saturating_mul(u128::from(sigma_multiplier));
        Some(
            self.samples
                .iter()
                .filter(|sample| sample.as_nanos().abs_diff(mean_ns) > threshold)
                .count(),
        )
    }

    /// Permille-indexed nearest-rank percentile — `permille = 500`
    /// returns the median, `permille = 999` the p99.9. Saturates on
    /// `permille > 1000`. `None` when the run has no successful
    /// samples.
    ///
    /// Integer-only API — sidesteps `float_arithmetic`. Callers that
    /// have an `f64` quantile must convert at the boundary
    /// (`(p * 1000.0).round() as u32`); the conversion is the caller's
    /// problem because the cleanest framing is to keep this surface
    /// integer-typed end-to-end. Use `percentile_permille(500)` for
    /// median, `(950)` for p95, `(999)` for p99.9, and so on.
    #[must_use]
    #[inline]
    pub fn percentile_permille(&self, permille: u32) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let permille_u128 = u128::from(permille.min(1000_u32));
        let n_u128 = u128::try_from(sorted.len()).unwrap_or(u128::MAX);
        // Nearest-rank: rank = ceil(permille · n / 1000) − 1, clamped.
        let numerator = permille_u128
            .saturating_mul(n_u128)
            .saturating_add(999_u128);
        let ceiling = numerator.checked_div(1000_u128).unwrap_or(0);
        let rank = usize::try_from(ceiling.saturating_sub(1_u128))
            .unwrap_or(usize::MAX)
            .min(sorted.len().saturating_sub(1));
        sorted.get(rank).copied()
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
    ///
    /// Computed in integer domain via the shortcut formula
    ///   n²·variance = n·Σx² − (Σx)²
    /// and `u128::isqrt` for the square root. Approximate to within
    /// 1ns due to integer-rounding in the final division.
    #[must_use]
    #[inline]
    pub fn std_dev(&self) -> Option<Duration> {
        if self.samples.len() < 2 {
            return None;
        }
        let n_u128 = u128::try_from(self.samples.len()).unwrap_or(u128::MAX);
        if n_u128 == 0_u128 {
            return None;
        }
        let total_nanos: u128 = self.samples.iter().map(Duration::as_nanos).sum();
        let sum_sq: u128 = self
            .samples
            .iter()
            .map(Duration::as_nanos)
            .map(|nanos| nanos.saturating_mul(nanos))
            .fold(0_u128, u128::saturating_add);
        let n_sum_sq = n_u128.saturating_mul(sum_sq);
        let total_sq = total_nanos.saturating_mul(total_nanos);
        let var_times_n_sq = n_sum_sq.saturating_sub(total_sq);
        let sd_times_n = var_times_n_sq.isqrt();
        let sd_nanos = sd_times_n.checked_div(n_u128).unwrap_or(0);
        Some(Duration::from_nanos(
            u64::try_from(sd_nanos).unwrap_or(u64::MAX),
        ))
    }

    /// A single-line summary: `"min X, p50 Y, p95 Z, max W (N samples)"`.
    #[must_use]
    #[inline]
    pub fn summary_line(&self) -> String {
        let n = self.samples.len();
        if n == 0 {
            return format!("no successful samples (iterations={})", self.iterations);
        }
        let min_text = fmt_duration(self.min().unwrap_or_default());
        let median_text = fmt_duration(self.median().unwrap_or_default());
        let p95_text = fmt_duration(self.percentile_permille(950_u32).unwrap_or_default());
        let max_text = fmt_duration(self.max().unwrap_or_default());
        format!("min {min_text}, p50 {median_text}, p95 {p95_text}, max {max_text} ({n} samples)")
    }

    /// Throughput in successful iterations per second × 1000 (i.e.
    /// milli-iterations per second). Returns the integer-encoded form so
    /// callers can format `xxx.yyy iter/s` without crossing the
    /// `float_arithmetic` line.
    ///
    /// `None` when the run hasn't completed (`total_elapsed == 0`) or no
    /// successful samples landed. The u32 representation caps at
    /// ≈ 4.29 M iter/s × 1000 = 4.29 G milli-iter/s — well above any
    /// real benchmark.
    ///
    /// [`strategy::Concurrent`]: crate::bench::strategy::Concurrent
    #[must_use]
    #[inline]
    pub fn throughput_milli_per_sec(&self) -> Option<u32> {
        let elapsed_nanos = self.total_elapsed.as_nanos();
        if elapsed_nanos == 0_u128 || self.samples.is_empty() {
            return None;
        }
        // throughput × 1000 = samples × 10^12 / elapsed_nanos.
        let samples_u128 = u128::try_from(self.samples.len()).unwrap_or(u128::MAX);
        let throughput_milli = samples_u128
            .saturating_mul(1_000_000_000_000_u128)
            .checked_div(elapsed_nanos)
            .unwrap_or(0_u128);
        Some(u32::try_from(throughput_milli).unwrap_or(u32::MAX))
    }
}
