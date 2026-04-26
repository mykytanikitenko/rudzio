use std::convert::Infallible;
use std::fmt;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::Config;
use crate::context;
use crate::runtime::{JoinError, Runtime};

use super::test::Test;

pub struct Suite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Root cancellation token whose children are handed out to per-test contexts.
    cancel: CancellationToken,
    /// Borrow of the async runtime driving the suite context.
    rt: &'suite_context R,
    /// Tracks spawned background tasks so teardown can wait for them.
    tracker: TaskTracker,
}

impl<'suite_context, R> fmt::Debug for Suite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Suite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> Suite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    #[inline]
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'suite_context,
        F::Output: 'static,
    {
        self.rt.block_on(fut)
    }

    #[inline]
    #[must_use]
    pub const fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    #[inline]
    pub fn spawn<F>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'suite_context
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        self.rt.spawn(fut)
    }

    #[inline]
    pub fn spawn_blocking<F, T>(
        &self,
        func: F,
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'suite_context
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.rt.spawn_blocking(func)
    }

    #[inline]
    pub fn spawn_local<F>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, JoinError>> + 'suite_context
    where
        F: Future + 'static,
    {
        self.rt.spawn_local(fut)
    }

    #[inline]
    #[must_use]
    pub const fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }

    #[inline]
    pub fn yield_now(&self) -> impl Future<Output = ()> + 'suite_context {
        self.rt.yield_now()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for Suite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = Test<'test_context, R>
    where
        Self: 'test_context;

    #[inline]
    fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> impl Future<Output = Result<Self::Test<'test_context>, Self::ContextError>> + Send + 'test_context
    {
        // Use the per-test token the runner supplies directly — it is already
        // a child of the root cancel token the suite context was built with,
        // so root-level cancellation still propagates.
        let tracker = self.tracker.clone();
        async move { Ok(Test::new(cancel, self.rt, tracker, config)) }
    }

    #[inline]
    async fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        // Root cancel is a child of the runner's token so that run-level
        // cancellation (timeout, SIGINT, SIGTERM) propagates to every test.
        Ok(Self {
            cancel: cancel.child_token(),
            rt,
            tracker: TaskTracker::new(),
        })
    }

    #[inline]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        let _closed: bool = self.tracker.close();
        self.tracker.wait().await;
        Ok(())
    }
}
