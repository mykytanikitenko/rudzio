//! Exercises the per-test branch where `Suite::context(...)` returns `Err`.
//!
//! Per the macro: every test whose context creation fails is counted as
//! Failed (not Panicked — that's reserved for setup failure) and the run
//! exits with code 1.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;

/// Error type used to fail context creation on purpose.
#[derive(Debug)]
struct ContextErr;

impl fmt::Display for ContextErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("context_creation_failed_by_design")
    }
}

impl Error for ContextErr {}

/// Suite context that always fails to produce a per-test context.
struct BrokenContextSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for BrokenContextSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrokenContextSuite")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for BrokenContextSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = ContextErr;
    type SetupError = ContextErr;
    type TeardownError = ContextErr;
    type Test<'test_context>
        = NeverBuiltTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Err(ContextErr)
    }

    async fn setup(_rt: &'suite_context R, _cancel: ::rudzio::tokio_util::sync::CancellationToken, _config: &'suite_context ::rudzio::Config) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
        })
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Test context placeholder; never actually constructed because
/// [`BrokenContextSuite::context`] always errors.
struct NeverBuiltTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for NeverBuiltTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeverBuiltTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for NeverBuiltTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = ContextErr;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = BrokenContextSuite,
        test = NeverBuiltTest,
    ),
])]
mod tests {
    use super::NeverBuiltTest;

    #[rudzio::test]
    fn first(_ctx: &NeverBuiltTest) -> anyhow::Result<()> {
        unreachable!("body must not run when context() fails")
    }

    #[rudzio::test]
    fn second(_ctx: &NeverBuiltTest) -> anyhow::Result<()> {
        unreachable!("body must not run when context() fails")
    }
}

#[rudzio::main]
fn main() {}
