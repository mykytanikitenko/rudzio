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
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;

static SUITE_SETUP_CALLS: AtomicUsize = AtomicUsize::new(0);
static SUITE_TEARDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);
static TEST_SETUP_CALLS: AtomicUsize = AtomicUsize::new(0);
static TEST_TEARDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);
static TEST_TRACKER_SPAWNED: AtomicUsize = AtomicUsize::new(0);
static TEST_TRACKER_COMPLETED: AtomicUsize = AtomicUsize::new(0);
static SUITE_TRACKER_SPAWNED: AtomicUsize = AtomicUsize::new(0);
static SUITE_TRACKER_COMPLETED: AtomicUsize = AtomicUsize::new(0);

struct InvariantsSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    suite_tracker: TaskTracker,
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> std::fmt::Debug for InvariantsSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvariantsSuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R>
    for InvariantsSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = InvariantsTest<'test_context, R>
    where
        Self: 'test_context;

    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        let _ = SUITE_SETUP_CALLS.fetch_add(1, Ordering::SeqCst);
        let tracker = TaskTracker::new();
        // Spawn one suite-level tracked task that sleeps briefly. If
        // Suite::teardown doesn't drain the tracker, the counter won't
        // tick before the FINAL line is printed.
        let _ = SUITE_TRACKER_SPAWNED.fetch_add(1, Ordering::SeqCst);
        let t = tracker.clone();
        let _join = t.spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let _ = SUITE_TRACKER_COMPLETED.fetch_add(1, Ordering::SeqCst);
        });
        Ok(Self {
            suite_tracker: tracker,
            _marker: PhantomData,
        })
    }

    async fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        let _ = TEST_SETUP_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(InvariantsTest {
            test_tracker: TaskTracker::new(),
            _marker: PhantomData,
        })
    }

    async fn teardown(self, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<(), Self::TeardownError> {
        let _ = self.suite_tracker.close();
        self.suite_tracker.wait().await;
        let _ = SUITE_TEARDOWN_CALLS.fetch_add(1, Ordering::SeqCst);
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

struct InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    test_tracker: TaskTracker,
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> std::fmt::Debug for InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvariantsTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn spawn_tracked_sleep(&self) {
        let _ = TEST_TRACKER_SPAWNED.fetch_add(1, Ordering::SeqCst);
        let _join = self.test_tracker.spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let _ = TEST_TRACKER_COMPLETED.fetch_add(1, Ordering::SeqCst);
        });
    }
}

impl<'test_context, R> context::Test<'test_context, R>
    for InvariantsTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(self, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<(), Self::TeardownError> {
        let _ = self.test_tracker.close();
        self.test_tracker.wait().await;
        let _ = TEST_TEARDOWN_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
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
    fn panicking(ctx: &InvariantsTest) -> anyhow::Result<()> {
        ctx.spawn_tracked_sleep();
        panic!("panicking_by_design")
    }
}

#[rudzio::main]
fn main() {}
