//! Hand-rolled `Suite` / `Test` implementation showing suite-level
//! shared state (a counter) that per-test contexts see.
//!
//! ```sh
//! cargo run --example custom_context
//! ```

use std::convert::Infallible;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

/// Suite-level state: a shared counter bumped every time a per-test
/// context is produced.
struct CountingSuite<'suite_context, Rt>
where
    Rt: Runtime<'suite_context> + Sync,
{
    /// Per-suite cancellation token.
    cancel: CancellationToken,
    /// Borrow of the async runtime driving this suite.
    rt: &'suite_context Rt,
    /// Counter incremented once per per-test context emitted by
    /// `context()`. Each `CountingTest` reads its own 1-based ordinal
    /// from this counter.
    tests_created: AtomicUsize,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
}

impl<'suite_context, Rt> fmt::Debug for CountingSuite<'suite_context, Rt>
where
    Rt: Runtime<'suite_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingSuite")
            .field("tests_created", &self.tests_created.load(Ordering::SeqCst))
            .finish()
    }
}

impl<'suite_context, Rt> context::Suite<'suite_context, Rt> for CountingSuite<'suite_context, Rt>
where
    Rt: for<'any> Runtime<'any> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = CountingTest<'test_context, Rt>
    where
        Self: 'test_context;

    #[inline]
    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        // Each test body sees its own 1-based ordinal, and the suite's
        // counter keeps climbing across the whole group.
        let nth = self
            .tests_created
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        Ok(CountingTest::new(
            cancel,
            config,
            nth,
            self.rt,
            self.tracker.clone(),
        ))
    }

    #[inline]
    fn rt(&self) -> &'suite_context Rt {
        self.rt
    }

    async fn setup(
        rt: &'suite_context Rt,
        cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            cancel: cancel.child_token(),
            rt,
            tests_created: AtomicUsize::new(0),
            tracker: TaskTracker::new(),
        })
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }

    #[inline]
    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

/// Per-test context, handed the ordinal the suite assigned. Marked
/// `#[non_exhaustive]` so external callers go through the constructor;
/// new fields can be added later without breaking downstream code.
#[non_exhaustive]
pub struct CountingTest<'test_context, Rt>
where
    Rt: Runtime<'test_context> + Sync,
{
    /// Per-test cancellation token.
    cancel: CancellationToken,
    /// Resolved CLI/env configuration.
    config: &'test_context Config,
    /// 1-based ordinal of this per-test context within the suite,
    /// assigned by `CountingSuite::context()`.
    nth: usize,
    /// Borrow of the async runtime driving this test.
    rt: &'test_context Rt,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
}

impl<'test_context, Rt> CountingTest<'test_context, Rt>
where
    Rt: Runtime<'test_context> + Sync,
{
    /// Construct a `CountingTest` with the given suite-assigned
    /// ordinal. Exposed so the suite's `context()` (which lives in
    /// the same crate as this struct in the example) can build one.
    #[inline]
    #[must_use]
    pub const fn new(
        cancel: CancellationToken,
        config: &'test_context Config,
        nth: usize,
        rt: &'test_context Rt,
        tracker: TaskTracker,
    ) -> Self {
        Self {
            cancel,
            config,
            nth,
            rt,
            tracker,
        }
    }

    /// 1-based ordinal of this test within its suite.
    #[inline]
    #[must_use]
    pub const fn nth(&self) -> usize {
        self.nth
    }
}

impl<'test_context, Rt> fmt::Debug for CountingTest<'test_context, Rt>
where
    Rt: Runtime<'test_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingTest")
            .field("nth", &self.nth)
            .finish()
    }
}

impl<'test_context, Rt> context::Test<'test_context, Rt> for CountingTest<'test_context, Rt>
where
    Rt: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    #[inline]
    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    #[inline]
    fn config(&self) -> &Config {
        self.config
    }

    #[inline]
    fn rt(&self) -> &'test_context Rt {
        self.rt
    }

    #[inline]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }

    #[inline]
    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = CountingSuite, test = CountingTest),
])]
mod tests {
    use super::CountingTest;

    #[rudzio::test]
    async fn first(ctx: &CountingTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.nth() >= 1);
        Ok(())
    }

    #[rudzio::test]
    async fn second(ctx: &CountingTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.nth() >= 1);
        Ok(())
    }

    #[rudzio::test]
    async fn third(ctx: &CountingTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.nth() >= 1);
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
