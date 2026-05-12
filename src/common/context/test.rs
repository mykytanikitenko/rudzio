use std::convert::Infallible;
use std::fmt;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::Config;
use crate::context;
use crate::runtime::Runtime;

pub struct Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Child cancellation token scoped to this test.
    cancel: CancellationToken,
    /// Resolved CLI/env configuration, handed down from the suite.
    config: &'test_context Config,
    /// Borrow of the async runtime driving the test.
    rt: &'test_context R,
    /// Shared task tracker inherited from the suite context.
    tracker: TaskTracker,
}

impl<'test_context, R> Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Construct a per-test context bundle.
    pub(crate) const fn new(
        cancel: CancellationToken,
        rt: &'test_context R,
        tracker: TaskTracker,
        config: &'test_context Config,
    ) -> Self {
        Self {
            cancel,
            config,
            rt,
            tracker,
        }
    }
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

impl<'test_context, R> context::Test<'test_context, R> for Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
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
    fn rt(&self) -> &'test_context R {
        self.rt
    }

    #[inline]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        Ok(())
    }

    #[inline]
    fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }
}
