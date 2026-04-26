//! Primitive [`Strategy`] implementations.
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

use super::{BenchProgressSnapshot, BenchReport, Strategy};
use crate::test_case::BoxError;

/// Run the body `N` times sequentially, awaiting each iteration before
/// starting the next.
///
/// Use when you want a clean latency distribution without contention —
/// every iteration gets the full runtime to itself and samples reflect
/// the body's isolated cost. `Sequential(1)` degenerates to a single
/// extra invocation, useful as a smoke-bench baseline.
#[derive(Debug, Clone, Copy)]
pub struct Sequential(pub usize);

impl Strategy for Sequential {
    #[inline]
    async fn run<B, Fut, P>(&self, mut body: B, mut on_progress: P) -> BenchReport
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
        P: FnMut(BenchProgressSnapshot),
    {
        let iterations = self.0;
        let stride = (iterations / 100).max(1);
        let mut samples = Vec::with_capacity(iterations);
        let mut failures = Vec::new();
        let mut panics: usize = 0;
        let start = Instant::now();
        // Iter 0: paint [BENCH] tag immediately, before any sample lands.
        on_progress(BenchProgressSnapshot::initial(iterations));
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
            if done % stride == 0 || done == iterations {
                on_progress(BenchProgressSnapshot::from_samples(
                    &samples, done, iterations,
                ));
            }
        }
        BenchReport {
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
pub struct Concurrent(pub usize);

impl Strategy for Concurrent {
    #[inline]
    async fn run<B, Fut, P>(&self, mut body: B, mut on_progress: P) -> BenchReport
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
        P: FnMut(BenchProgressSnapshot),
    {
        let iterations = self.0;
        let stride = (iterations / 100).max(1);
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
        on_progress(BenchProgressSnapshot::initial(iterations));
        while let Some((iter_elapsed, result)) = in_flight.next().await {
            match result {
                Ok(Ok(())) => samples.push(iter_elapsed),
                Ok(Err(err)) => failures.push(err.to_string()),
                Err(_payload) => panics = panics.saturating_add(1),
            }
            done = done.saturating_add(1);
            if done.is_multiple_of(stride) || done == iterations {
                on_progress(BenchProgressSnapshot::from_samples(
                    &samples, done, iterations,
                ));
            }
        }
        BenchReport {
            strategy: format!("Concurrent({iterations})"),
            iterations,
            samples,
            failures,
            panics,
            total_elapsed: start.elapsed(),
        }
    }
}
