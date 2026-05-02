//! Composable benchmark [`Strategy`] trait plus primitive implementations.
//!
//! [`Sequential`] runs the body N times one after another; [`Concurrent`]
//! produces N futures and drives them via `futures::join_all` on the same
//! task (cooperative concurrency, not thread-parallelism). Both are
//! stateless — holding them behind `&self` in `run` is zero-cost and
//! allows their literals (`Sequential(1000)`) to be evaluated directly at
//! the `benchmark = ...` attribute site.

use std::iter;
use std::panic::AssertUnwindSafe;
use std::time::Instant;

use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;

use super::{ProgressSnapshot, Report};
use crate::test_case::BoxError;

/// Composable benchmark strategy.
///
/// A strategy decides how many times, and with what concurrency, to call
/// the test body. `body` is a closure that produces a fresh future per
/// call; the strategy invokes it repeatedly and aggregates per-iteration
/// timings into a [`Report`].
///
/// The trait is deliberately minimal: writing a new strategy is just a
/// new `impl`. Composition (warm-up then measure, repeat K rounds,
/// sequence A-then-B) is a matter of wrapping one or more inner
/// strategies in a new type and delegating. No runtime registry, no
/// magic — whatever the user writes at `benchmark = <expr>` is the
/// concrete type the macro-generated code calls `.run(...)` on.
pub trait Strategy {
    /// Run the body according to this strategy, collecting per-iteration
    /// timings into a [`Report`].
    ///
    /// `body` is called afresh for every iteration — the future it
    /// returns is polled to completion (or panic) inside a
    /// [`std::panic::catch_unwind`] boundary so one bad iteration
    /// doesn't abort the whole bench.
    ///
    /// `on_progress` is invoked at strategy entry (with a zero-progress
    /// placeholder so the live-region renderer can flip the row tag
    /// from `[RUN]` to `[BENCH]` immediately) and roughly every 1% of
    /// iterations thereafter, with the latest [`ProgressSnapshot`].
    /// Implementations that omit progress should still call it once
    /// at entry — a `|_| ()` no-op closure is acceptable from callers
    /// that don't care.
    fn run<B, Fut, P>(&self, body: B, on_progress: P) -> impl Future<Output = Report>
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
        P: FnMut(ProgressSnapshot);
}

/// Run the body `N` times sequentially, awaiting each iteration before
/// starting the next.
///
/// Use when you want a clean latency distribution without contention —
/// every iteration gets the full runtime to itself and samples reflect
/// the body's isolated cost. `Sequential::new(1)` degenerates to a
/// single extra invocation, useful as a smoke-bench baseline.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Sequential(pub usize);

impl Sequential {
    /// Construct a `Sequential` strategy that runs the body `iterations`
    /// times one after another.
    #[inline]
    #[must_use]
    pub const fn new(iterations: usize) -> Self {
        Self(iterations)
    }
}

impl Strategy for Sequential {
    #[inline]
    async fn run<B, Fut, P>(&self, mut body: B, mut on_progress: P) -> Report
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
        P: FnMut(ProgressSnapshot),
    {
        let iterations = self.0;
        let stride = iterations.checked_div(100).unwrap_or(0).max(1);
        let mut samples = Vec::with_capacity(iterations);
        let mut failures = Vec::new();
        let mut panics: usize = 0;
        let start = Instant::now();
        // Iter 0: paint [BENCH] tag immediately, before any sample lands.
        on_progress(ProgressSnapshot::initial(iterations));
        for idx in 0..iterations {
            let fut = body();
            let iter_start = Instant::now();
            let result = AssertUnwindSafe(fut).catch_unwind().await;
            let iter_elapsed = iter_start.elapsed();
            match result {
                Ok(Ok(())) => samples.push(iter_elapsed),
                Ok(Err(err)) => failures.push(err.to_string()),
                Err(_payload) => panics = panics.saturating_add(1),
            }
            let done = idx.saturating_add(1);
            if done.checked_rem(stride).is_some_and(|rem| rem == 0) || done == iterations {
                on_progress(ProgressSnapshot::from_samples(&samples, done, iterations));
            }
        }
        Report {
            strategy: format!("Sequential({iterations})"),
            iterations,
            samples,
            failures,
            panics,
            total_elapsed: start.elapsed(),
        }
    }
}

/// Produce `N` futures up front and drive them concurrently on the same
/// task via [`futures_util::future::join_all`].
///
/// The strategy is scheduler-level concurrency, not thread parallelism —
/// every iteration shares one executor task. That's what makes it cheap
/// (no spawn, no Send bound) and lets it work under `!Send` runtimes
/// (compio, embassy, tokio `Local`, futures `LocalPool`). Per-iteration
/// latency is measured from the moment each inner future is first polled,
/// so samples capture time spent awaiting I/O / timers / other
/// iterations rather than the cost of setting up the N-way fan-out.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Concurrent(pub usize);

impl Concurrent {
    /// Construct a `Concurrent` strategy that drives `iterations` copies
    /// of the body on the same task.
    #[inline]
    #[must_use]
    pub const fn new(iterations: usize) -> Self {
        Self(iterations)
    }
}

impl Strategy for Concurrent {
    #[inline]
    async fn run<B, Fut, P>(&self, mut body: B, mut on_progress: P) -> Report
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
        P: FnMut(ProgressSnapshot),
    {
        let iterations = self.0;
        let stride = iterations.checked_div(100).unwrap_or(0).max(1);
        let start = Instant::now();
        // body() is `FnMut`; we call it sequentially up front, then
        // drive the resulting futures via `FuturesUnordered` so we can
        // emit progress per-completion rather than waiting for the full
        // join. `iter_start` is captured inside each wrapped future so
        // the sample measures the body's actual polling time, not the
        // fan-out overhead.
        let mut in_flight: FuturesUnordered<_> = iter::repeat_with(|| {
            let fut = body();
            async move {
                let iter_start = Instant::now();
                let result = AssertUnwindSafe(fut).catch_unwind().await;
                (iter_start.elapsed(), result)
            }
        })
        .take(iterations)
        .collect();

        let mut samples = Vec::with_capacity(iterations);
        let mut failures = Vec::new();
        let mut panics: usize = 0;
        let mut done: usize = 0;
        on_progress(ProgressSnapshot::initial(iterations));
        while let Some((iter_elapsed, result)) = in_flight.next().await {
            match result {
                Ok(Ok(())) => samples.push(iter_elapsed),
                Ok(Err(err)) => failures.push(err.to_string()),
                Err(_payload) => panics = panics.saturating_add(1),
            }
            done = done.saturating_add(1);
            if done.is_multiple_of(stride) || done == iterations {
                on_progress(ProgressSnapshot::from_samples(&samples, done, iterations));
            }
        }
        Report {
            strategy: format!("Concurrent({iterations})"),
            iterations,
            samples,
            failures,
            panics,
            total_elapsed: start.elapsed(),
        }
    }
}
