use std::fmt;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::runtime::Runtime;

/// Per-runtime shared state.
///
/// Lifetime hierarchy: runtime > `suite_context` > `test_context`
/// - '`suite_context`: lifetime of this suite context (bounded by runtime)
/// - Test context lifetime is tied to borrow of Suite
///
/// Flow:
/// 1. Create runtime (lives for 'runtime)
/// 2. Create suite context from runtime (lives for '`suite_context` ≤ 'runtime)
/// 3. For each test: borrow suite, create test context (lives for borrow duration)
/// 4. Run test with test context
/// 5. Teardown test context
/// 6. Release borrow on suite
/// 7. Teardown suite
/// 8. Drop runtime
pub trait Suite<'suite_context, R>: Send + Sync + 'suite_context
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    /// Error returned by [`Self::context`].
    type ContextError: fmt::Display + fmt::Debug + Send + Sync + 'suite_context;

    /// Error returned by [`Self::setup`].
    type SetupError: fmt::Display + fmt::Debug + Send + Sync + 'suite_context;

    /// Error returned by [`Self::teardown`].
    type TeardownError: fmt::Display + fmt::Debug + Send + Sync + 'suite_context;

    /// The per-test context type produced by [`Self::context`].
    ///
    /// Generic over `'test_context` — the third tier of the hierarchy
    /// `'runtime: 'suite_context: 'test_context`. The test value is born
    /// when the runner calls [`Self::context`] and dies when its
    /// `Test::teardown` finishes, so its data may borrow from Suite only
    /// for that strictly-shorter window. Encoding that here (rather than
    /// reusing `'suite_context`) lets the runner give the test fn a
    /// `&'test_context (mut) Self::Test<'test_context>` borrow whose
    /// lifetime is genuinely the per-test borrow — needed for any test fn
    /// that takes `&mut TestCtx` to compile.
    type Test<'test_context>: super::Test<'test_context, R>
    where
        Self: 'test_context;

    /// Create a fresh per-test context.
    ///
    /// `cancel` is a per-test cancellation token provided by the runner, a
    /// child of the root token the suite context was constructed with.
    /// Implementations should hand it to [`Self::Test`] so per-test timeouts
    /// (or root-level cancels that fan out through the parent/child chain)
    /// propagate into the test body.
    ///
    /// `config` mirrors what [`Self::setup`] received. Passed explicitly
    /// so `Test` impls can store it and surface it to test bodies without
    /// reaching through `rt.config()`.
    ///
    /// `'test_context` is the duration of the `&self` borrow that produces
    /// the test — the test runner's own per-test stack frame.
    fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> impl Future<Output = Result<Self::Test<'test_context>, Self::ContextError>> + Send + 'test_context;

    /// Create the shared state. Called once per runtime group.
    ///
    /// `cancel` is the runner's root cancellation token for this run. The
    /// runner cancels it on a run-wide timeout or on SIGINT/SIGTERM;
    /// implementations should keep it (or a child of it) so per-test contexts
    /// inherit the cancellation.
    ///
    /// `config` is the resolved run configuration (CLI flags + env snapshot),
    /// mirroring [`Runtime::config`](crate::runtime::Runtime::config). Passed
    /// directly so `setup` bodies don't need to reach through `rt` for it.
    fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        config: &'suite_context Config,
    ) -> impl Future<Output = Result<Self, Self::SetupError>> + Send + 'suite_context
    where
        Self: Sized;

    /// Tear down the shared state. Called after all tests in the group.
    ///
    /// `cancel` is a per-phase cancellation token (a child of the
    /// runner's root token). The runner cancels it on either the
    /// suite-teardown timeout or a parent cancel (run-timeout, SIGINT).
    /// Cooperative impls should poll it to bail out promptly instead of
    /// running to completion past the timeout.
    fn teardown(
        self,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), Self::TeardownError>> + Send + 'suite_context;
}
