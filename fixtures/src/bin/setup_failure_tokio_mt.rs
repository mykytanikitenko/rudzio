//! Exercises the branch where `Suite::setup` returns `Err`.
//!
//! Per the macro: the runtime thread emits a `[FAIL] setup <suite>`
//! lifecycle line carrying the error's `Display`, every test in that
//! group is reported as `Cancelled` (it never got to run), and the
//! process exits with code 1.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;

/// Error type used to fail suite setup on purpose.
#[derive(Debug)]
struct SetupFailed;

impl fmt::Display for SetupFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("setup_failed_by_design")
    }
}

impl Error for SetupFailed {}

/// Suite context whose [`context::Suite::setup`] always errors.
struct FailingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for FailingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailingSuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for FailingSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
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
        _cancel: CancellationToken,
        _config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Err(SetupFailed)
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        Err(SetupFailed)
    }

    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Test context placeholder; never actually constructed because
/// [`FailingSuite::setup`] always errors.
struct NeverBuilt<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for NeverBuilt<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeverBuilt").finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for NeverBuilt<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = SetupFailed;

    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture asserts Suite::setup's Err is reported and tests are Cancelled; the never_runs() body trivially returns Ok(()) so its anyhow::Result<()> wrapper is redundant, but the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = FailingSuite,
        test = NeverBuilt,
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
