use std::convert::Infallible;
use std::fmt;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use rudzio::context;
use rudzio::runtime::{JoinError, Runtime};

use crate::test_context::Test;

pub struct Global<'context_global, R>
where
    R: Runtime<'context_global> + Sync,
{
    /// Root cancellation token whose children are handed out to per-test contexts.
    cancel: CancellationToken,
    /// Borrow of the async runtime driving the global context.
    rt: &'context_global R,
    /// Tracks spawned background tasks so teardown can wait for them.
    tracker: TaskTracker,
}

impl<'context_global, R> fmt::Debug for Global<'context_global, R>
where
    R: Runtime<'context_global> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Global").finish_non_exhaustive()
    }
}

impl<'context_global, R> Global<'context_global, R>
where
    R: Runtime<'context_global> + Sync,
{
    #[inline]
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'context_global,
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
    ) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'context_global
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
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'context_global
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
    ) -> impl Future<Output = Result<F::Output, JoinError>> + 'context_global
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
    pub fn yield_now(&self) -> impl Future<Output = ()> + 'context_global {
        self.rt.yield_now()
    }
}

impl<'context_global, R> context::Global<'context_global, R> for Global<'context_global, R>
where
    R: for<'r> Runtime<'r> + Sync,
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
    ) -> impl Future<Output = Result<Self::Test<'test_context>, Self::ContextError>>
           + Send
           + 'test_context {
        // Use the per-test token the runner supplies directly — it is already
        // a child of the root cancel token the global context was built with,
        // so root-level cancellation still propagates.
        let tracker = self.tracker.clone();
        async move {
            Ok(Test {
                cancel,
                rt: self.rt,
                tracker,
            })
        }
    }

    #[inline]
    async fn setup(
        rt: &'context_global R,
        cancel: CancellationToken,
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
    async fn teardown(self) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        let _closed: bool = self.tracker.close();
        self.tracker.wait().await;
        Ok(())
    }
}
