//! Hand-rolled `context::Suite` / `context::Test` implementations,
//! independent of the `rudzio::common::context` module.
//!
//! Guards two things:
//!   - the public trait surface in `rudzio::context` stays implementable
//!     downstream without depending on our helper module;
//!   - the name-collision fix (`use ::rudzio::context::Test as _;` inside the
//!     macro output) still works when the user names their struct `MySuite`
//!     / `MyTest` rather than `Suite` / `Test`.
//!
//! Also exercises a non-`anyhow` error type as the `SetupError` /
//! `ContextError` / `TeardownError` associated types.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;

/// Sentinel error type that never occurs in practice.
#[derive(Debug)]
struct NeverFails;

impl fmt::Display for NeverFails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NeverFails")
    }
}

impl Error for NeverFails {}

/// Custom suite context with no shared state beyond a runtime borrow.
struct MySuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for MySuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MySuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for MySuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = NeverFails;
    type SetupError = NeverFails;
    type TeardownError = NeverFails;
    type Test<'test_context>
        = MyTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(MyTest {
            _marker: PhantomData,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
        })
    }

    async fn teardown(self, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Custom per-test context with no state.
struct MyTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for MyTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MyTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for MyTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = NeverFails;

    async fn teardown(self, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = MySuite,
        test = MyTest,
    ),
])]
mod tests {
    use super::MyTest;

    #[rudzio::test]
    fn runs_on_custom_context(_ctx: &MyTest) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
