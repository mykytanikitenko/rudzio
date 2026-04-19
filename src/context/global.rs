use std::fmt;

use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

/// Per-runtime shared state.
///
/// Lifetime hierarchy: runtime > `context_global` > `test_context`
/// - '`context_global`: lifetime of this global context (bounded by runtime)
/// - Test context lifetime is tied to borrow of Global
///
/// Flow:
/// 1. Create runtime (lives for 'runtime)
/// 2. Create global context from runtime (lives for '`context_global` ≤ 'runtime)
/// 3. For each test: borrow global, create test context (lives for borrow duration)
/// 4. Run test with test context
/// 5. Teardown test context
/// 6. Release borrow on global
/// 7. Teardown global
/// 8. Drop runtime
pub trait Global<'context_global, R>: Send + Sync + 'context_global
where
    R: for<'r> Runtime<'r> + Sync,
{
    /// Error returned by [`Self::context`].
    type ContextError: fmt::Display + fmt::Debug + Send + Sync + 'context_global;

    /// Error returned by [`Self::setup`].
    type SetupError: fmt::Display + fmt::Debug + Send + Sync + 'context_global;

    /// Error returned by [`Self::teardown`].
    type TeardownError: fmt::Display + fmt::Debug + Send + Sync + 'context_global;

    /// The per-test context type produced by [`Self::context`].
    ///
    /// Generic over `'test_context` — the third tier of the hierarchy
    /// `'runtime: 'context_global: 'test_context`. The test value is born
    /// when the runner calls [`Self::context`] and dies when its
    /// `Test::teardown` finishes, so its data may borrow from Global only
    /// for that strictly-shorter window. Encoding that here (rather than
    /// reusing `'context_global`) lets the runner give the test fn a
    /// `&'test_context (mut) Self::Test<'test_context>` borrow whose
    /// lifetime is genuinely the per-test borrow — needed for any test fn
    /// that takes `&mut TestCtx` to compile.
    type Test<'test_context>: super::Test<'test_context, R>
    where
        Self: 'test_context;

    /// Create a fresh per-test context.
    ///
    /// `cancel` is a per-test cancellation token provided by the runner, a
    /// child of the root token the global context was constructed with.
    /// Implementations should hand it to [`Self::Test`] so per-test timeouts
    /// (or root-level cancels that fan out through the parent/child chain)
    /// propagate into the test body.
    ///
    /// `'test_context` is the duration of the `&self` borrow that produces
    /// the test — the test runner's own per-test stack frame.
    fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<Self::Test<'test_context>, Self::ContextError>>
           + Send
           + 'test_context;

    /// Create the shared state. Called once per runtime group.
    ///
    /// `cancel` is the runner's root cancellation token for this run. The
    /// runner cancels it on a global run-timeout or on SIGINT/SIGTERM;
    /// implementations should keep it (or a child of it) so per-test contexts
    /// inherit the cancellation.
    fn setup(
        rt: &'context_global R,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<Self, Self::SetupError>> + Send + 'context_global
    where
        Self: Sized;

    /// Tear down the shared state. Called after all tests in the group.
    fn teardown(
        self,
    ) -> impl Future<Output = Result<(), Self::TeardownError>> + Send + 'context_global;
}
