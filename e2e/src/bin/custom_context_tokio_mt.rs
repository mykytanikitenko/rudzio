//! Hand-rolled `context::Global` / `context::Test` implementations,
//! independent of the `common-context` crate.
//!
//! Guards two things:
//!   - the public trait surface in `rudzio::context` stays implementable
//!     downstream without depending on our helper crate;
//!   - the name-collision fix (`use ::rudzio::context::Test as _;` inside the
//!     macro output) still works when the user names their struct `MyGlobal`
//!     / `MyTest` rather than `Global` / `Test`.
//!
//! Also exercises a non-`anyhow` error type as the `SetupError` /
//! `ContextError` / `TeardownError` associated types.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;

/// Sentinel error type that never occurs in practice.
#[derive(Debug)]
struct NeverFails;

impl fmt::Display for NeverFails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NeverFails")
    }
}

impl Error for NeverFails {}

/// Custom global context with no shared state beyond a runtime borrow.
struct MyGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'cg R>,
}

impl<'cg, R> fmt::Debug for MyGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MyGlobal").finish_non_exhaustive()
    }
}

impl<'cg, R> context::Global<'cg, R> for MyGlobal<'cg, R>
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
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(MyTest {
            _marker: PhantomData,
        })
    }

    async fn setup(_rt: &'cg R, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
        })
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Custom per-test context with no state.
struct MyTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'tc R>,
}

impl<'tc, R> fmt::Debug for MyTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MyTest").finish_non_exhaustive()
    }
}

impl<'tc, R> context::Test<'tc, R> for MyTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    type TeardownError = NeverFails;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = MyGlobal,
        test_context = MyTest,
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
