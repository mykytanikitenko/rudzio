//! Exercise: two `#[rudzio::suite]` blocks declaring the same
//! `(runtime, suite, test)` tuple must collapse into one group →
//! exactly one `Suite::setup` + one `Suite::teardown` per runtime.
//!
//! The test framework itself is responsible for the grouping; the
//! user shouldn't have to merge their `mod`s. If the counter ends up
//! at 2 instead of 1, rudzio is emitting a separate group for each
//! `#[rudzio::suite]` block even when their keys collide, and the
//! framework has a bug to fix.

use std::convert::Infallible;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

/// Number of `Suite::setup` invocations observed across all groups.
static SETUP_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Number of `Suite::teardown` invocations observed across all groups.
static TEARDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Suite context counting `setup`/`teardown` invocations to assert
/// duplicate `(runtime, suite, test)` tuples collapse into one group.
struct CountingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Per-suite cancellation token.
    cancel: CancellationToken,
    /// Borrow of the async runtime driving this suite.
    rt: &'suite_context R,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
}

/// Per-test context with no state; the test bodies inspect
/// [`SETUP_CALLS`] directly.
struct CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Per-test cancellation token.
    cancel: CancellationToken,
    /// Resolved CLI/env configuration.
    config: &'test_context Config,
    /// Borrow of the async runtime driving this test.
    rt: &'test_context R,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
}

impl<'suite_context, R> fmt::Debug for CountingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingSuite").finish_non_exhaustive()
    }
}

impl<'test_context, R> fmt::Debug for CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingTest").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for CountingSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = CountingTest<'test_context, R>
    where
        Self: 'test_context;

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(CountingTest {
            cancel,
            config,
            rt: self.rt,
            tracker: self.tracker.clone(),
        })
    }

    fn rt(&self) -> &'suite_context R {
        self.rt
    }

    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts duplicate suite tuples collapse into one group by emitting machine-readable COUNTING_SUITE_SETUP lines that the integration test greps; println! is the deliberate channel"
    )]
    async fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        let prev = SETUP_CALLS.fetch_add(1_usize, Ordering::SeqCst);
        println!(
            "COUNTING_SUITE_SETUP (new count: {})",
            prev.saturating_add(1_usize),
        );
        Ok(Self {
            cancel: cancel.child_token(),
            rt,
            tracker: TaskTracker::new(),
        })
    }

    #[expect(
        clippy::print_stdout,
        reason = "this fixture asserts duplicate suite tuples collapse into one group by emitting machine-readable COUNTING_SUITE_TEARDOWN lines that the integration test greps; println! is the deliberate channel"
    )]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        let prev = TEARDOWN_CALLS.fetch_add(1_usize, Ordering::SeqCst);
        println!(
            "COUNTING_SUITE_TEARDOWN (new count: {})",
            prev.saturating_add(1_usize),
        );
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

impl<'test_context, R> context::Test<'test_context, R> for CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    fn config(&self) -> &Config {
        self.config
    }

    fn rt(&self) -> &'test_context R {
        self.rt
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }

    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = CountingSuite,
        test = CountingTest,
    ),
])]
mod first_mod {
    use super::{CountingTest, SETUP_CALLS};
    use std::sync::atomic::Ordering;

    #[rudzio::test]
    fn in_first_mod(_ctx: &CountingTest) -> anyhow::Result<()> {
        let count = SETUP_CALLS.load(Ordering::SeqCst);
        anyhow::ensure!(
            count == 1,
            "setup must have run exactly once when both mods share \
             the same (runtime, suite, test) tuple; observed {count}",
        );
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = CountingSuite,
        test = CountingTest,
    ),
])]
mod second_mod {
    use super::{CountingTest, SETUP_CALLS};
    use std::sync::atomic::Ordering;

    #[rudzio::test]
    fn in_second_mod(_ctx: &CountingTest) -> anyhow::Result<()> {
        let count = SETUP_CALLS.load(Ordering::SeqCst);
        anyhow::ensure!(
            count == 1,
            "setup must have run exactly once when both mods share \
             the same (runtime, suite, test) tuple; observed {count}",
        );
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
