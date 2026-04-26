//! Reproduction of rust-lang/rust#100013 from user's drop-barrier integration.
//!
//! Before the lifetime fix, the `.await` on a runtime-borrowed future inside
//! a `#[rudzio::test]` body caused rustc to emit "lifetime bound not
//! satisfied" pointing at the `#[rudzio::suite]` attribute — because the
//! macro hard-coded `'static` into the generated helper fns.

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudzio::context;
use rudzio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Counts how many times `BaseSuite::setup` runs across the whole process.
/// Two `#[rudzio::suite]` blocks declaring the same `(R, S)` should share
/// one suite; with two runtime kinds (Multithread + CurrentThread) we
/// expect this to land at exactly 2.
static SETUP_CALLS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct NeverFails;

impl fmt::Display for NeverFails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NeverFails")
    }
}

impl Error for NeverFails {}

struct BaseSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    cancel: CancellationToken,
    rt: &'suite_context R,
    tracker: TaskTracker,
}

impl<'suite_context, R> fmt::Debug for BaseSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseSuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for BaseSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = NeverFails;
    type SetupError = NeverFails;
    type TeardownError = NeverFails;
    type Test<'test_context>
        = BaseTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(BaseTest {
            cancel,
            rt: self.rt,
            tracker: self.tracker.clone(),
        })
    }

    async fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        let _ = SETUP_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            cancel,
            rt,
            tracker: TaskTracker::new(),
        })
    }

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        let _ = self.tracker.close();
        self.tracker.wait().await;
        Ok(())
    }
}

struct BaseTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    cancel: CancellationToken,
    rt: &'test_context R,
    tracker: TaskTracker,
}

impl<'test_context, R> fmt::Debug for BaseTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for BaseTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = NeverFails;

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = BaseSuite,
        test = BaseTest,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = BaseSuite,
        test = BaseTest,
    ),
])]
mod scenarios {
    use super::BaseTest;

    #[rudzio::test]
    async fn ctx_yield_now(ctx: &BaseTest) -> anyhow::Result<()> {
        ctx.rt.yield_now().await;
        Ok(())
    }

    #[rudzio::test]
    async fn ctx_sleep(ctx: &BaseTest) -> anyhow::Result<()> {
        ctx.rt.sleep(std::time::Duration::from_millis(0)).await;
        Ok(())
    }

    #[rudzio::test]
    async fn ctx_spawn(ctx: &BaseTest) -> anyhow::Result<()> {
        let handle = ctx.rt.spawn(ctx.tracker.track_future(async { 7_u32 }));
        assert_eq!(handle.await.ok(), Some(7));
        Ok(())
    }

    #[rudzio::test]
    async fn no_ctx_use(_ctx: &BaseTest) -> anyhow::Result<()> {
        Ok(())
    }
}

// Second suite block declaring the same (Multithread + CurrentThread, BaseSuite)
// pair — must coalesce with `scenarios` and share one runtime + one suite per
// runtime kind, not spin up fresh ones.
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = BaseSuite,
        test = BaseTest,
    ),
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = BaseSuite,
        test = BaseTest,
    ),
])]
mod sharing {
    use super::BaseTest;

    #[rudzio::test]
    async fn shares_runtime_with_scenarios(_ctx: &BaseTest) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    async fn shares_global_with_scenarios(ctx: &BaseTest) -> anyhow::Result<()> {
        ctx.rt.yield_now().await;
        Ok(())
    }

    /// Asserts BaseSuite::setup ran at most twice across the whole process
    /// (once per (R, G) pair: Multithread+BaseSuite and CurrentThread+BaseSuite).
    /// If two blocks with the same (R, G) didn't coalesce, this would be 4.
    #[rudzio::test]
    async fn global_setup_was_shared(_ctx: &BaseTest) -> anyhow::Result<()> {
        let calls = super::SETUP_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(
            calls <= 2,
            "expected ≤2 setup calls (one per runtime kind); got {calls}",
        );
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
