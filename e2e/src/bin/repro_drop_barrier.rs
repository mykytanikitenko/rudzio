//! Reproduction of rust-lang/rust#100013 from user's drop-barrier integration.
//!
//! Before the lifetime fix, the `.await` on a runtime-borrowed future inside
//! a `#[rudzio::test]` body caused rustc to emit "lifetime bound not
//! satisfied" pointing at the `#[rudzio::suite]` attribute — because the
//! macro hard-coded `'static` into the generated helper fns.
#![allow(dead_code)]

use std::error::Error;
use std::fmt;

use rudzio::context;
use rudzio::runtime::tokio::{CurrentThread, Multithread};
use rudzio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[derive(Debug)]
struct NeverFails;

impl fmt::Display for NeverFails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NeverFails")
    }
}

impl Error for NeverFails {}

struct BaseGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    cancel: CancellationToken,
    rt: &'cg R,
    tracker: TaskTracker,
}

impl<'cg, R> fmt::Debug for BaseGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseGlobal").finish_non_exhaustive()
    }
}

impl<'cg, R> context::Global<'cg, R> for BaseGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    type ContextError = NeverFails;
    type SetupError = NeverFails;
    type TeardownError = NeverFails;
    type Test = BaseTest<'cg, R>;

    async fn context(&self, cancel: CancellationToken) -> Result<Self::Test, Self::ContextError> {
        Ok(BaseTest {
            cancel,
            rt: self.rt,
            tracker: self.tracker.clone(),
        })
    }

    async fn setup(rt: &'cg R, cancel: CancellationToken) -> Result<Self, Self::SetupError> {
        Ok(Self {
            cancel,
            rt,
            tracker: TaskTracker::new(),
        })
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        let _ = self.tracker.close();
        self.tracker.wait().await;
        Ok(())
    }
}

struct BaseTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    cancel: CancellationToken,
    rt: &'tc R,
    tracker: TaskTracker,
}

impl<'tc, R> fmt::Debug for BaseTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseTest").finish_non_exhaustive()
    }
}

impl<'tc, R> context::Test<'tc, R> for BaseTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    type TeardownError = NeverFails;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = BaseGlobal,
        test_context = BaseTest,
    ),
    (
        runtime = CurrentThread::new,
        global_context = BaseGlobal,
        test_context = BaseTest,
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
        let handle = ctx.rt.spawn(async { 7_u32 });
        assert_eq!(handle.await.ok(), Some(7));
        Ok(())
    }

    #[rudzio::test]
    async fn no_ctx_use(_ctx: &BaseTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
