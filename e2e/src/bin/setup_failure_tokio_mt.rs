//! Exercises the FATAL branch where `Global::setup` returns `Err`.
//!
//! Per the macro: the runtime thread logs "FATAL: failed to create global
//! context", every test in that group is counted as panicked, and the
//! process exits with code 1.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;

/// Error type used to fail global setup on purpose.
#[derive(Debug)]
struct SetupFailed;

impl fmt::Display for SetupFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("setup_failed_by_design")
    }
}

impl Error for SetupFailed {}

/// Global context whose [`context::Global::setup`] always errors.
struct FailingGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'cg R>,
}

impl<'cg, R> fmt::Debug for FailingGlobal<'cg, R>
where
    R: Runtime<'cg> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailingGlobal").finish_non_exhaustive()
    }
}

impl<'cg, R> context::Global<'cg, R> for FailingGlobal<'cg, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = SetupFailed;
    type SetupError = SetupFailed;
    type TeardownError = SetupFailed;
    type Test<'test_context>
        = NeverBuilt<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Err(SetupFailed)
    }

    async fn setup(_rt: &'cg R, _cancel: ::rudzio::tokio_util::sync::CancellationToken) -> Result<Self, Self::SetupError> {
        Err(SetupFailed)
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Test context placeholder; never actually constructed because
/// [`FailingGlobal::setup`] always errors.
struct NeverBuilt<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'tc R>,
}

impl<'tc, R> fmt::Debug for NeverBuilt<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeverBuilt").finish_non_exhaustive()
    }
}

impl<'tc, R> context::Test<'tc, R> for NeverBuilt<'tc, R>
where
    R: Runtime<'tc> + Sync,
{
    type TeardownError = SetupFailed;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = FailingGlobal,
        test_context = NeverBuilt,
    ),
])]
mod tests {
    use super::NeverBuilt;

    #[rudzio::test]
    fn never_runs(_ctx: &NeverBuilt) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
