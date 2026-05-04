use std::fmt;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::Config;
use crate::runtime::{JoinError, Runtime};

/// Per-test context.
///
/// Created by [`super::Suite::context`] before each test.
/// Single lifetime parameter '`test_context` - everything in this context lives for '`test_context`.
/// The '`test_context` lifetime is the duration of the borrow from Suite.
///
/// User-facing API methods (`block_on`, `spawn`, `spawn_blocking`,
/// `spawn_local`, `spawn_tracked`, `sleep`, `yield_now`, `name`,
/// `config`) are provided as default implementations that mirror the
/// suite's API surface and pass through to [`Runtime`] via
/// [`Self::rt`]. The chain is `Test → (mirrors Suite) → Runtime`: a
/// test body calling `ctx.sleep(d)` reaches the runtime through the
/// same delegation Suite uses, with no separate adapter-specific path.
/// A concrete implementor only needs to supply the accessors
/// ([`Self::rt`], [`Self::cancel_token`], [`Self::tracker`],
/// [`Self::config`]) plus the [`Self::teardown`] hook.
pub trait Test<'test_context, R>: Send + 'test_context
where
    R: Runtime<'test_context> + Sync,
{
    /// Error returned by [`Self::teardown`].
    type TeardownError: fmt::Display + fmt::Debug + Send + Sync + 'test_context;

    /// Borrow of the runtime driving this test (the same runtime the
    /// enclosing suite was constructed with). The default-implemented
    /// API methods on this trait dispatch through this accessor.
    fn rt(&self) -> &'test_context R;

    /// Per-test cancellation token. The runner builds it as a child of
    /// the suite's root token and hands it to
    /// [`super::Suite::context`]; implementations should expose it here
    /// so test bodies and tracked background tasks can observe per-test
    /// cancellation (timeout, parent cancel).
    fn cancel_token(&self) -> &CancellationToken;

    /// Resolved CLI/env [`Config`] for this run — the same `&Config` the
    /// runtime constructor and `Suite::setup` / `Suite::context`
    /// received. Surfaced here so test bodies don't need to reach
    /// through `rt().config()`.
    fn config(&self) -> &Config;

    /// Shared task tracker inherited from the enclosing suite. Used by
    /// [`Self::spawn_tracked`] so suite teardown can wait for any
    /// background work the test started.
    fn tracker(&self) -> &TaskTracker;

    /// Tear down per-test state. Called after the test body returns.
    ///
    /// `cancel` is a per-phase cancellation token (a child of the
    /// runner's root token). The runner cancels it on either the
    /// per-test-teardown timeout or a parent cancel (run-timeout,
    /// SIGINT). Cooperative impls should poll it to bail out instead of
    /// running past the timeout.
    fn teardown(
        self,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), Self::TeardownError>> + Send + 'test_context;

    /// Block the calling thread until `fut` completes, dispatching
    /// through [`Runtime::block_on`].
    #[inline]
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'test_context,
        F::Output: 'static,
    {
        self.rt().block_on(fut)
    }

    /// Stable identifier of the runtime driving this test, surfaced
    /// from [`Runtime::name`].
    #[inline]
    fn name(&self) -> &'static str {
        self.rt().name()
    }

    /// Sleep for `duration` using the runtime's native timer
    /// ([`Runtime::sleep`]).
    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'test_context {
        self.rt().sleep(duration)
    }

    /// Spawn a `Send` future onto the runtime ([`Runtime::spawn`]).
    #[inline]
    fn spawn<F>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'test_context
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        self.rt().spawn(fut)
    }

    /// Spawn a blocking closure onto a thread suitable for blocking I/O
    /// ([`Runtime::spawn_blocking`]).
    #[inline]
    fn spawn_blocking<F, T>(
        &self,
        func: F,
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'test_context
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.rt().spawn_blocking(func)
    }

    /// Spawn a `!Send` future onto a thread-local executor
    /// ([`Runtime::spawn_local`]).
    #[inline]
    fn spawn_local<F>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, JoinError>> + 'test_context
    where
        F: Future + 'static,
    {
        self.rt().spawn_local(fut)
    }

    /// Spawn a `Send` future onto the runtime, tracked by the
    /// suite-shared [`TaskTracker`]. Suite teardown closes the tracker
    /// and waits for outstanding tracked tasks, so anything spawned
    /// here is guaranteed to drain before the suite goes away.
    #[inline]
    fn spawn_tracked<F>(
        &self,
        fut: F,
    ) -> impl Future<Output = Result<(), JoinError>> + Send + 'test_context
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.rt().spawn(self.tracker().track_future(fut))
    }

    /// Yield control back to the runtime scheduler
    /// ([`Runtime::yield_now`]).
    #[inline]
    fn yield_now(&self) -> impl Future<Output = ()> + 'test_context {
        self.rt().yield_now()
    }
}
