use std::fmt;

use crate::runtime::Runtime;

/// Per-test context.
///
/// Created by [`super::Suite::context`] before each test.
/// Single lifetime parameter '`test_context` - everything in this context lives for '`test_context`.
/// The '`test_context` lifetime is the duration of the borrow from Suite.
pub trait Test<'test_context, R>: Send + 'test_context
where
    R: Runtime<'test_context> + Sync,
{
    /// Error returned by [`Self::teardown`].
    type TeardownError: fmt::Display + fmt::Debug + Send + Sync + 'test_context;

    /// Tear down per-test state. Called after the test body returns.
    fn teardown(
        self,
    ) -> impl Future<Output = Result<(), Self::TeardownError>> + Send + 'test_context;
}
