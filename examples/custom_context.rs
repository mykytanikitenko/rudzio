//! Hand-rolled `Suite` / `Test` implementation showing suite-level
//! shared state (a counter) that per-test contexts see.
//!
//! ```sh
//! cargo run --example custom_context
//! ```

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::tokio_util::sync::CancellationToken;

/// Suite-level state: a shared counter bumped every time a per-test
/// context is produced.
struct CountingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    tests_created: AtomicUsize,
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for CountingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingSuite")
            .field("tests_created", &self.tests_created.load(Ordering::SeqCst))
            .finish()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for CountingSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = CountingTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        // Each test body sees its own 1-based ordinal, and the suite's
        // counter keeps climbing across the whole group.
        let nth = self.tests_created.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(CountingTest {
            nth,
            _marker: PhantomData,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            tests_created: AtomicUsize::new(0),
            _marker: PhantomData,
        })
    }

    async fn teardown(
        self,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

/// Per-test context, handed the ordinal the suite assigned.
pub struct CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    pub nth: usize,
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingTest")
            .field("nth", &self.nth)
            .finish()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for CountingTest<'test_context, R>
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
        suite = CountingSuite,
        test = CountingTest,
    ),
])]
mod tests {
    use super::CountingTest;

    #[rudzio::test]
    async fn first(ctx: &CountingTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.nth >= 1);
        Ok(())
    }

    #[rudzio::test]
    async fn second(ctx: &CountingTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.nth >= 1);
        Ok(())
    }

    #[rudzio::test]
    async fn third(ctx: &CountingTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.nth >= 1);
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
