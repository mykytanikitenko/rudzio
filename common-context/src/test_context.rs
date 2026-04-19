use std::convert::Infallible;
use std::fmt;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use rudzio::context;
use rudzio::runtime::{JoinError, Runtime};

pub struct Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Child cancellation token scoped to this test.
    pub(crate) cancel: CancellationToken,
    /// Borrow of the async runtime driving the test.
    pub(crate) rt: &'test_context R,
    /// Shared task tracker inherited from the global context.
    pub(crate) tracker: TaskTracker,
}

impl<'test_context, R> fmt::Debug for Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Test").finish_non_exhaustive()
    }
}

impl<'test_context, R> Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    #[inline]
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'test_context,
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
    ) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'test_context
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
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'test_context
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
    ) -> impl Future<Output = Result<F::Output, JoinError>> + 'test_context
    where
        F: Future + 'static,
    {
        self.rt.spawn_local(fut)
    }

    #[inline]
    pub fn spawn_tracked<F>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<(), JoinError>> + Send + 'test_context
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(self.tracker.track_future(fut))
    }

    #[inline]
    #[must_use]
    pub const fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }

    #[inline]
    pub fn yield_now(&self) -> impl Future<Output = ()> + 'test_context {
        self.rt.yield_now()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    #[inline]
    async fn teardown(self) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        Ok(())
    }
}
