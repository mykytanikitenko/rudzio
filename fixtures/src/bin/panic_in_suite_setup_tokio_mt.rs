//! Exercises the `catch_unwind` wrapper around `Suite::setup`.
//!
//! Setup panics rather than returning Err. Without the wrapper, the
//! panic would unwind through the runtime thread and the runner's
//! join handler would surface a generic "runtime thread panicked"
//! diagnostic with no link to the suite. The wrapper turns it into
//! a structured `[FAIL]   setup` lifecycle line carrying the panic
//! message, and every test in the group is reported as Cancelled
//! (it never had a chance to run).

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;

/// Suite whose [`context::Suite::setup`] always panics.
struct PanickingSetupSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for PanickingSetupSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PanickingSetupSuite")
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for PanickingSetupSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = NeverBuilt<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        unreachable!("context() must not run when setup panicked")
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        panic!("suite_setup_panicked_by_design")
    }

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

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
    type TeardownError = Infallible;

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = PanickingSetupSuite,
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
