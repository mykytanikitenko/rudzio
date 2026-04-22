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

use std::time::Duration;

use crate::test_case::BoxError;

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
    /// Human-readable strategy label, e.g. `"Sequential(1000)"`.
    pub strategy: String,
    /// Total number of iterations the strategy attempted.
    pub iterations: usize,
    /// Elapsed time of every iteration that returned `Ok(())`.
    pub samples: Vec<Duration>,
    /// Error strings from iterations that returned `Err(_)`.
    pub failures: Vec<String>,
    /// Count of iterations whose future panicked mid-poll.
    pub panics: usize,
    /// Wall-clock time the whole strategy run took.
    pub total_elapsed: Duration,
}

impl BenchReport {
    /// `true` when every iteration completed without errors or panics.
    #[inline]
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.failures.is_empty() && self.panics == 0
    }

    /// Smallest successful-iteration duration, or `None` if every iteration
    /// failed or the strategy attempted zero iterations.
    #[must_use]
    pub fn min(&self) -> Option<Duration> {
        self.samples.iter().copied().min()
    }

    /// Largest successful-iteration duration, or `None` when there are no
    /// successful samples.
    #[must_use]
    pub fn max(&self) -> Option<Duration> {
        self.samples.iter().copied().max()
    }

    /// Arithmetic mean of successful-iteration durations, or `None` when
    /// there are no successful samples.
    #[must_use]
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

    /// Sample at the `p`-th percentile (`0.0..=1.0`, nearest-rank) or
    /// `None` when there are no successful samples.
    ///
    /// `percentile(0.5)` is the median; `percentile(0.99)` is the p99.
    /// Returns `None` when `p` is outside `[0.0, 1.0]`.
    #[must_use]
    pub fn percentile(&self, p: f64) -> Option<Duration> {
        if !(0.0..=1.0).contains(&p) || self.samples.is_empty() {
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
        let rank = ((p * sorted.len() as f64).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len().saturating_sub(1));
        Some(sorted[rank])
    }

    /// Sample at the median (50th percentile), or `None` when empty.
    #[inline]
    #[must_use]
    pub fn median(&self) -> Option<Duration> {
        self.percentile(0.5)
    }

    /// A single-line summary: `"min X, p50 Y, p95 Z, max W (N samples)"`.
    #[must_use]
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

    /// Render a horizontal ASCII histogram with `buckets` bars of `width`
    /// characters each.
    ///
    /// Returns an empty string when there are no successful samples; the
    /// range is `[min, max]` split into equal-width linear buckets. Each
    /// line is `"  [lo..hi) |######  count"`.
    #[must_use]
    pub fn ascii_histogram(&self, buckets: usize, width: usize) -> String {
        if self.samples.is_empty() || buckets == 0 {
            return String::new();
        }
        let min_ns = self.min().unwrap_or_default().as_nanos();
        let max_ns = self.max().unwrap_or_default().as_nanos();
        let span = max_ns.saturating_sub(min_ns).max(1);
        let bucket_span = span.div_ceil(buckets as u128).max(1);

        let mut counts = vec![0_usize; buckets];
        for s in &self.samples {
            let offset = s.as_nanos().saturating_sub(min_ns);
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
            out.push_str(&format!(
                "  [{lo:>9.2?} .. {hi:>9.2?}) |{bar:<width$}  {count}\n"
            ));
        }
        out
    }
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
    fn run<B, Fut>(&self, body: B) -> impl Future<Output = BenchReport>
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>;
}
