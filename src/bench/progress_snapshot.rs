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
    ///
    /// # Errors
    ///
    /// Propagates an `anyhow` error if any of the integer-domain
    /// statistics (percentile rank, coefficient of variation, histogram
    /// binning) overflow or underflow. For realistic benchmark durations
    /// these steps are infallible, but keeping them fallible means a
    /// corrupted sample set surfaces loudly instead of silently
    /// saturating into a wrong summary.
    #[inline]
    pub fn from_samples(samples: &[Duration], done: usize, total: usize) -> anyhow::Result<Self> {
        if samples.is_empty() {
            let mut snap = Self::initial(total);
            snap.done = done;
            return Ok(snap);
        }
        let mut sorted: Vec<Duration> = samples.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        let min = sorted
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("progress snapshot: no samples"))?;
        let max = sorted
            .last()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("progress snapshot: no samples"))?;

        let n_u128 = u128::try_from(n).map_err(|_ignored| {
            anyhow::anyhow!("progress snapshot: sample count overflows u128")
        })?;
        let p50 = sorted
            .get(percentile_index(n, n_u128, 500_u32)?)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("progress snapshot: p50 index out of range"))?;
        let p95 = sorted
            .get(percentile_index(n, n_u128, 950_u32)?)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("progress snapshot: p95 index out of range"))?;
        let cov_permille = coefficient_of_variation(&sorted, n, n_u128)?;
        let histogram = bin_histogram(&sorted, min, max)?;

        Ok(Self {
            cov_permille,
            done,
            histogram,
            max,
            min,
            p50,
            p95,
            total,
        })
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
        let DistSummary {
            cov_permille,
            histogram,
            max,
            min,
            p50,
            p95,
        } = stats;
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

/// Compute the `permille`-th nearest-rank percentile index into a sorted
/// sample slice of length `n`, in pure integer math (`rank = ceil(p·n) −
/// 1`, expressed as `ceil(permille·n/1000) − 1`). `permille` is `percent ×
/// 10` (e.g. `500` = p50, `950` = p95).
fn percentile_index(n: usize, n_u128: u128, permille: u32) -> anyhow::Result<usize> {
    let permille_u128 = u128::from(permille);
    let numerator = permille_u128
        .checked_mul(n_u128)
        .and_then(|value| value.checked_add(999_u128))
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: percentile numerator overflow"))?;
    let ceiling = numerator
        .checked_div(1000_u128)
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: percentile division by 1000"))?;
    let rank_before_clamp = ceiling
        .checked_sub(1_u128)
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: percentile rank underflow"))?;
    let upper_bound = n
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: percentile rank underflow"))?;
    Ok(usize::try_from(rank_before_clamp)
        .map_err(|_ignored| anyhow::anyhow!("progress snapshot: percentile rank overflows usize"))?
        .min(upper_bound))
}

/// Coefficient of variation σ/mean computed entirely in integer domain via
/// the shortcut formula `cov² = (n·Σx² − (Σx)²) / (Σx)²`, carried as
/// parts-per-thousand (cov × 1000) in a `u16`. We never widen to floats —
/// the renderer formats `xx.x%` directly out of the permille integer.
/// `None` when `n < 2` or the mean is zero.
fn coefficient_of_variation(
    sorted: &[Duration],
    n: usize,
    n_u128: u128,
) -> anyhow::Result<Option<u16>> {
    if n < 2 {
        return Ok(None);
    }
    let total_nanos: u128 = sorted.iter().map(Duration::as_nanos).sum();
    if total_nanos == 0_u128 {
        return Ok(None);
    }
    let mut sum_sq: u128 = 0;
    for nanos in sorted.iter().map(Duration::as_nanos) {
        sum_sq = sum_sq
            .checked_add(
                nanos
                    .checked_mul(nanos)
                    .ok_or_else(|| anyhow::anyhow!("progress snapshot: sample squared overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("progress snapshot: sum of squares overflow"))?;
    }
    let total_sq = total_nanos
        .checked_mul(total_nanos)
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: total squared overflow"))?;
    let n_sum_sq = n_u128
        .checked_mul(sum_sq)
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: n * sum of squares overflow"))?;
    // Variance numerator (n·Σx² − (Σx)²); 0 when all samples equal.
    let var_numerator = n_sum_sq
        .checked_sub(total_sq)
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: variance numerator underflow"))?;
    // cov² × 10^6 = var_numerator × 10^6 / (Σx)².
    let cov_milli_sq = var_numerator
        .checked_mul(1_000_000_u128)
        .and_then(|value| value.checked_div(total_sq))
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: cov computation overflow"))?;
    let cov_milli_u128 = cov_milli_sq.isqrt();
    Ok(Some(u16::try_from(cov_milli_u128).map_err(|_ignored| {
        anyhow::anyhow!("progress snapshot: cov overflows u16")
    })?))
}

/// Linear binning over `[min, max]` into `HISTOGRAM_BUCKETS` buckets.
fn bin_histogram(
    sorted: &[Duration],
    min: Duration,
    max: Duration,
) -> anyhow::Result<[u32; HISTOGRAM_BUCKETS]> {
    let mut histogram = [0_u32; HISTOGRAM_BUCKETS];
    let min_ns = min.as_nanos();
    let max_ns = max.as_nanos();
    let span = max_ns
        .checked_sub(min_ns)
        .ok_or_else(|| anyhow::anyhow!("progress snapshot: histogram span underflow"))?
        .max(1);
    let buckets_u128 = u128::try_from(HISTOGRAM_BUCKETS)
        .map_err(|_ignored| anyhow::anyhow!("progress snapshot: bucket count overflows u128"))?;
    let bucket_span = span.div_ceil(buckets_u128).max(1);
    for sample in sorted {
        let offset = sample
            .as_nanos()
            .checked_sub(min_ns)
            .ok_or_else(|| anyhow::anyhow!("progress snapshot: sample below min"))?;
        let idx = usize::try_from(
            offset
                .checked_div(bucket_span)
                .ok_or_else(|| anyhow::anyhow!("progress snapshot: zero bucket span"))?,
        )
        .map_err(|_ignored| anyhow::anyhow!("progress snapshot: index overflows usize"))?
        .min(
            HISTOGRAM_BUCKETS
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("progress snapshot: zero buckets"))?,
        );
        if let Some(slot) = histogram.get_mut(idx) {
            *slot = slot
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("progress snapshot: histogram count overflow"))?;
        }
    }
    Ok(histogram)
}
