use std::convert::Infallible;
use std::fmt;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::Config;
use crate::context;
use crate::runtime::{JoinError, Runtime};

pub struct Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Child cancellation token scoped to this test.
    pub(super) cancel: CancellationToken,
    /// Borrow of the async runtime driving the test.
    pub(super) rt: &'test_context R,
    /// Shared task tracker inherited from the suite context.
    pub(super) tracker: TaskTracker,
    /// Resolved CLI/env configuration, handed down from the suite.
    pub(super) config: &'test_context Config,
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

    /// Resolved CLI/env [`Config`] for this run — the same `&Config` the
    /// runtime constructor and `Suite::setup` / `Suite::context` received.
    #[inline]
    #[must_use]
    pub const fn config(&self) -> &Config {
        self.config
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
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        Ok(())
    }
}
