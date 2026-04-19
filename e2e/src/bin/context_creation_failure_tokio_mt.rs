//! Exercises the per-test branch where `Global::context(...)` returns `Err`.
//!
//! Per the macro: every test whose context creation fails is counted as
//! Failed (not Panicked — that's reserved for setup failure) and the run
//! exits with code 1.

// Test bodies use `unreachable!` to assert they never execute when the
// framework short-circuits them via a failing context.
#![allow(
    clippy::unreachable,
    reason = "test fixture intentionally exercises unreachable branches"
)]

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;

/// Error type used to fail context creation on purpose.
#[derive(Debug)]
struct ContextErr;

impl fmt::Display for ContextErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("context_creation_failed_by_design")
    }
}

impl Error for ContextErr {}

/// Global context that always fails to produce a per-test context.
struct BrokenContextGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'cg R>,
}

impl<'cg, R> fmt::Debug for BrokenContextGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrokenContextGlobal")
            .finish_non_exhaustive()
    }
}

impl<'cg, R> context::Global<'cg, R> for BrokenContextGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    type ContextError = ContextErr;
    type SetupError = ContextErr;
    type TeardownError = ContextErr;
    type Test = NeverBuiltTest<'cg, R>;

    async fn context(&self, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<Self::Test, Self::ContextError> {
        Err(ContextErr)
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

/// Test context placeholder; never actually constructed because
/// [`BrokenContextGlobal::context`] always errors.
struct NeverBuiltTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'tc R>,
}

impl<'tc, R> fmt::Debug for NeverBuiltTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeverBuiltTest").finish_non_exhaustive()
    }
}

impl<'tc, R> context::Test<'tc, R> for NeverBuiltTest<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    type TeardownError = ContextErr;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = BrokenContextGlobal,
        test_context = NeverBuiltTest,
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
