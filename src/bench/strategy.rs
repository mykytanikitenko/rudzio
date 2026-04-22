//! Primitive [`Strategy`] implementations.
//!
//! [`Sequential`] runs the body N times one after another; [`Concurrent`]
//! produces N futures and drives them via `futures::join_all` on the same
//! task (cooperative concurrency, not thread-parallelism). Both are
//! stateless — holding them behind `&self` in `run` is zero-cost and
//! allows their literals (`Sequential(1000)`) to be evaluated directly at
//! the `benchmark = ...` attribute site.

use std::panic::AssertUnwindSafe;
use std::time::Instant;

use futures_util::FutureExt as _;

use super::{BenchReport, Strategy};
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
    async fn run<B, Fut>(&self, mut body: B) -> BenchReport
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
    {
        let iterations = self.0;
        let mut samples = Vec::with_capacity(iterations);
        let mut failures = Vec::new();
        let mut panics: usize = 0;
        let start = Instant::now();
        for _ in 0..iterations {
            let fut = body();
            let iter_start = Instant::now();
            let result = AssertUnwindSafe(fut).catch_unwind().await;
            let iter_elapsed = iter_start.elapsed();
            match result {
                Ok(Ok(())) => samples.push(iter_elapsed),
                Ok(Err(e)) => failures.push(e.to_string()),
                Err(_payload) => panics = panics.saturating_add(1),
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
    async fn run<B, Fut>(&self, mut body: B) -> BenchReport
    where
        B: FnMut() -> Fut,
        Fut: Future<Output = Result<(), BoxError>>,
    {
        let iterations = self.0;
        let start = Instant::now();
        // body() is `FnMut`; we call it sequentially up front, then
        // drive the resulting futures concurrently. `iter_start` is
        // captured inside each wrapped future so the sample measures
        // the body's actual polling time, not the fan-out overhead.
        let futures: Vec<_> = (0..iterations)
            .map(|_| {
                let fut = body();
                async move {
                    let iter_start = Instant::now();
                    let result = AssertUnwindSafe(fut).catch_unwind().await;
                    (iter_start.elapsed(), result)
                }
            })
            .collect();
        let results = futures_util::future::join_all(futures).await;

        let mut samples = Vec::with_capacity(iterations);
        let mut failures = Vec::new();
        let mut panics: usize = 0;
        for (iter_elapsed, result) in results {
            match result {
                Ok(Ok(())) => samples.push(iter_elapsed),
                Ok(Err(e)) => failures.push(e.to_string()),
                Err(_payload) => panics = panics.saturating_add(1),
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
