//! Asserts two invariants rudzio must never break:
//!
//! 1. `Test::teardown` runs exactly once per test, regardless of how
//!    the test body ends — pass, return Err, panic. (Timeout is
//!    covered by the separate `teardown_always_runs_tokio_mt`
//!    fixture; `Suite::context` failure produces no context, so
//!    there is nothing to tear down and it is not counted here.)
//! 2. Both suite-level and per-test `TaskTracker`s drain before
//!    their respective `teardown` returns. Tasks spawned via the
//!    tracker must be observed as completed by the teardown.
//!
//! The fixture runs three tests producing three different outcomes
//! (pass, fail, panic) and carries atomic counters + per-test
//! `TaskTracker` spawns. `Suite::teardown` prints a
//! machine-readable invariants-check line the integration test
//! greps.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;
use tokio::time::sleep;

/// Count of `Suite::setup` invocations.
static SUITE_SETUP_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Count of `Suite::teardown` invocations.
static SUITE_TEARDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Count of completed suite-tracker tasks observed before
/// `Suite::teardown` returns.
static SUITE_TRACKER_COMPLETED: AtomicUsize = AtomicUsize::new(0);
/// Count of suite-tracker tasks spawned during `Suite::setup`.
static SUITE_TRACKER_SPAWNED: AtomicUsize = AtomicUsize::new(0);
/// Count of `Suite::context` invocations.
static TEST_SETUP_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Count of `Test::teardown` invocations.
static TEST_TEARDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Count of completed per-test-tracker tasks observed before
/// `Test::teardown` returns.
static TEST_TRACKER_COMPLETED: AtomicUsize = AtomicUsize::new(0);
/// Count of per-test-tracker tasks spawned during a test body.
static TEST_TRACKER_SPAWNED: AtomicUsize = AtomicUsize::new(0);

/// Suite context carrying the suite-level `TaskTracker` so its
/// `teardown` can drain it.
struct InvariantsSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
    /// Suite-level tracker; populated in `setup`, closed + drained in
    /// `teardown`.
    suite_tracker: TaskTracker,
}

/// Per-test context carrying its own `TaskTracker` so its `teardown`
/// can drain it independently of the suite tracker.
struct InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
    /// Per-test tracker; populated by `spawn_tracked_sleep`, closed +
    /// drained in `teardown`.
    test_tracker: TaskTracker,
}

impl<'suite_context, R> fmt::Debug for InvariantsSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvariantsSuite").finish_non_exhaustive()
    }
}

impl<'test_context, R> fmt::Debug for InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvariantsTest").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for InvariantsSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = InvariantsTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        let _prev: usize = TEST_SETUP_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(InvariantsTest {
            test_tracker: TaskTracker::new(),
            _marker: PhantomData,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        let _prev: usize = SUITE_SETUP_CALLS.fetch_add(1, Ordering::SeqCst);
        let tracker = TaskTracker::new();
        // Spawn one suite-level tracked task that sleeps briefly. If
        // Suite::teardown doesn't drain the tracker, the counter won't
        // tick before the FINAL line is printed.
        let _prev_spawned: usize = SUITE_TRACKER_SPAWNED.fetch_add(1, Ordering::SeqCst);
        let tracker_clone = tracker.clone();
        let _join = tracker_clone.spawn(async move {
            sleep(Duration::from_millis(40)).await;
            let _prev_completed: usize =
                SUITE_TRACKER_COMPLETED.fetch_add(1, Ordering::SeqCst);
        });
        Ok(Self {
            suite_tracker: tracker,
            _marker: PhantomData,
        })
    }

    #[expect(
        clippy::print_stdout,
        reason = "this fixture verifies invariants by emitting a single machine-readable INVARIANTS_CHECK line that the integration test greps; using println! is the deliberate channel"
    )]
    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        let _closed: bool = self.suite_tracker.close();
        self.suite_tracker.wait().await;
        let _prev: usize = SUITE_TEARDOWN_CALLS.fetch_add(1, Ordering::SeqCst);
        println!(
            "INVARIANTS_CHECK suite_setup={} suite_teardown={} test_setup={} test_teardown={} test_tasks_spawned={} test_tasks_completed={} suite_tasks_spawned={} suite_tasks_completed={}",
            SUITE_SETUP_CALLS.load(Ordering::SeqCst),
            SUITE_TEARDOWN_CALLS.load(Ordering::SeqCst),
            TEST_SETUP_CALLS.load(Ordering::SeqCst),
            TEST_TEARDOWN_CALLS.load(Ordering::SeqCst),
            TEST_TRACKER_SPAWNED.load(Ordering::SeqCst),
            TEST_TRACKER_COMPLETED.load(Ordering::SeqCst),
            SUITE_TRACKER_SPAWNED.load(Ordering::SeqCst),
            SUITE_TRACKER_COMPLETED.load(Ordering::SeqCst),
        );
        Ok(())
    }
}

impl<'test_context, R> InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Spawn a tracked sleep that ticks the per-test "completed"
    /// counter once it finishes.
    fn spawn_tracked_sleep(&self) {
        let _prev_spawned: usize = TEST_TRACKER_SPAWNED.fetch_add(1, Ordering::SeqCst);
        let _join = self.test_tracker.spawn(async move {
            sleep(Duration::from_millis(40)).await;
            let _prev_completed: usize =
                TEST_TRACKER_COMPLETED.fetch_add(1, Ordering::SeqCst);
        });
    }
}

impl<'test_context, R> context::Test<'test_context, R> for InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        let _closed: bool = self.test_tracker.close();
        self.test_tracker.wait().await;
        let _prev: usize = TEST_TEARDOWN_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture asserts Test::teardown runs even when the body panics; the panicking() test body diverges so its anyhow::Result<()> wrapper is statically unreachable, and the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = InvariantsSuite,
        test = InvariantsTest,
    ),
])]
mod tests {
    use super::InvariantsTest;

    #[rudzio::test]
    fn passing(ctx: &InvariantsTest) -> anyhow::Result<()> {
        ctx.spawn_tracked_sleep();
        Ok(())
    }

    #[rudzio::test]
    fn failing(ctx: &InvariantsTest) -> anyhow::Result<()> {
        ctx.spawn_tracked_sleep();
        anyhow::bail!("failing_by_design")
    }

    #[rudzio::test]
    #[expect(
        clippy::panic,
        reason = "this fixture asserts Test::teardown runs even when the body panics; the body must panic to exercise that branch"
    )]
    fn panicking(ctx: &InvariantsTest) -> anyhow::Result<()> {
        ctx.spawn_tracked_sleep();
        panic!("panicking_by_design")
    }
}

#[rudzio::main]
fn main() {}
