//! Cheap, fixed-size benchmark progress summary emitted from
//! [`crate::bench::Strategy::run`] and consumed by the live-region
//! renderer.
//!
//! `Copy` so the snapshot travels through the lifecycle channel without
//! allocation. The histogram is pre-binned (linear over `[min, max]`)
//! so the drawer doesn't need to keep the raw per-iteration sample
//! vector around.

use std::time::Duration;

use crate::bench::dist_summary::DistSummary;

/// Number of linear histogram buckets carried in a
/// [`ProgressSnapshot`].
pub const HISTOGRAM_BUCKETS: usize = 32;

/// Cheap, fixed-size summary of a benchmark's progress.
///
/// Emitted from [`crate::bench::Strategy::run`] roughly every 1% of
/// iterations and consumed by the live-region renderer to draw a
/// progress bar, p50 / p95 / cov, and a mini-histogram below the
/// running row.
///
/// `Copy` so it travels through the lifecycle channel without
/// allocation. The histogram is pre-binned (linear over `[min, max]`)
/// so the drawer doesn't need to keep the raw per-iteration sample
/// vector around.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ProgressSnapshot {
    /// Coefficient of variation (σ / mean) carried as parts-per-thousand
    /// (e.g. `Some(43)` means cov ≈ 0.043). `None` when fewer than two
    /// samples are available or the mean is zero. Integer-encoded so the
    /// renderer can format `xx.x%` purely in integer math, sidestepping
    /// `float_arithmetic` everywhere downstream.
    pub cov_permille: Option<u16>,
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

impl ProgressSnapshot {
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
        let min = sorted.first().copied().unwrap_or(Duration::ZERO);
        let max = sorted.last().copied().unwrap_or(Duration::ZERO);

        // Nearest-rank percentile in integer math: rank = ceil(p * n) - 1,
        // expressed as ceil(permille * n / 1000) - 1 with permille = p * 1000.
        let n_u128 = u128::try_from(n).unwrap_or(u128::MAX);
        let rank = |permille: u32| -> usize {
            let permille_u128 = u128::from(permille);
            let numerator = permille_u128
                .saturating_mul(n_u128)
                .saturating_add(999_u128);
            let ceiling = numerator.checked_div(1000_u128).unwrap_or(0);
            usize::try_from(ceiling.saturating_sub(1_u128))
                .unwrap_or(usize::MAX)
                .min(n.saturating_sub(1))
        };
        let p50 = sorted.get(rank(500_u32)).copied().unwrap_or(Duration::ZERO);
        let p95 = sorted.get(rank(950_u32)).copied().unwrap_or(Duration::ZERO);

        // Coefficient of variation σ/mean computed entirely in integer
        // domain via the shortcut formula
        //   cov² = (n·Σx² − (Σx)²) / (Σx)²
        // and carried as parts-per-thousand (cov × 1000) in a u16. We
        // never widen to floats — the renderer formats `xx.x%` directly
        // out of the permille integer. `None` when n < 2 or mean = 0.
        let cov_permille = if n < 2 {
            None
        } else {
            let total_nanos: u128 = sorted.iter().map(Duration::as_nanos).sum();
            if total_nanos == 0_u128 {
                None
            } else {
                let sum_sq: u128 = sorted
                    .iter()
                    .map(Duration::as_nanos)
                    .map(|nanos| nanos.saturating_mul(nanos))
                    .fold(0_u128, u128::saturating_add);
                let total_sq = total_nanos.saturating_mul(total_nanos);
                let n_sum_sq = n_u128.saturating_mul(sum_sq);
                // Variance numerator (n·Σx² − (Σx)²); 0 when all samples equal.
                let var_numerator = n_sum_sq.saturating_sub(total_sq);
                // cov² × 10^6 = var_numerator × 10^6 / (Σx)².
                let cov_milli_sq = var_numerator
                    .saturating_mul(1_000_000_u128)
                    .checked_div(total_sq)
                    .unwrap_or(0_u128);
                let cov_milli_u128 = cov_milli_sq.isqrt();
                Some(u16::try_from(cov_milli_u128).unwrap_or(u16::MAX))
            }
        };

        // Linear binning over [min, max] into HISTOGRAM_BUCKETS bins.
        let mut histogram = [0_u32; HISTOGRAM_BUCKETS];
        let min_ns = min.as_nanos();
        let max_ns = max.as_nanos();
        let span = max_ns.saturating_sub(min_ns).max(1);
        let buckets_u128 = u128::try_from(HISTOGRAM_BUCKETS).unwrap_or(u128::MAX);
        let bucket_span = span.div_ceil(buckets_u128).max(1);
        for sample in &sorted {
            let offset = sample.as_nanos().saturating_sub(min_ns);
            let idx = usize::try_from(offset.checked_div(bucket_span).unwrap_or(0))
                .unwrap_or(usize::MAX)
                .min(HISTOGRAM_BUCKETS.saturating_sub(1));
            if let Some(slot) = histogram.get_mut(idx) {
                *slot = slot.saturating_add(1);
            }
        }

        Self {
            cov_permille,
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
            cov_permille: None,
            histogram: [0_u32; HISTOGRAM_BUCKETS],
        }
    }

    /// Construct a `ProgressSnapshot` from progress counters and a
    /// pre-computed [`DistSummary`] block. Bundled to keep the
    /// constructor signature short — see [`DistSummary`] for the
    /// distribution-summary fields.
    #[inline]
    #[must_use]
    pub const fn new(done: usize, total: usize, stats: DistSummary) -> Self {
        let DistSummary { cov_permille, histogram, max, min, p50, p95 } = stats;
        Self {
            cov_permille,
            done,
            histogram,
            max,
            min,
            p50,
            p95,
            total,
        }
    }
}
