//! Exercise: two `#[rudzio::suite]` blocks declaring the same
//! `(runtime, suite, test)` tuple must collapse into one group →
//! exactly one `Suite::setup` + one `Suite::teardown` per runtime.
//!
//! The test framework itself is responsible for the grouping; the
//! user shouldn't have to merge their `mod`s. If the counter ends up
//! at 2 instead of 1, rudzio is emitting a separate group for each
//! `#[rudzio::suite]` block even when their keys collide, and the
//! framework has a bug to fix.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudzio::context;
use rudzio::runtime::Runtime;

static SETUP_CALLS: AtomicUsize = AtomicUsize::new(0);
static TEARDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);

struct CountingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for CountingSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingSuite").finish_non_exhaustive()
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
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(CountingTest {
            _marker: PhantomData,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: ::rudzio::tokio_util::sync::CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        let prev = SETUP_CALLS.fetch_add(1, Ordering::SeqCst);
        println!("COUNTING_SUITE_SETUP (new count: {})", prev + 1);
        Ok(Self {
            _marker: PhantomData,
        })
    }

    async fn teardown(self) -> Result<(), Self::TeardownError> {
        let prev = TEARDOWN_CALLS.fetch_add(1, Ordering::SeqCst);
        println!("COUNTING_SUITE_TEARDOWN (new count: {})", prev + 1);
        Ok(())
    }
}

struct CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    _marker: PhantomData<&'test_context R>,
}

impl<'test_context, R> fmt::Debug for CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingTest").finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for CountingTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    async fn teardown(self) -> Result<(), Self::TeardownError> {
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
mod first_mod {
    use super::{CountingTest, SETUP_CALLS};
    use std::sync::atomic::Ordering;

    #[rudzio::test]
    fn in_first_mod(_ctx: &CountingTest) -> anyhow::Result<()> {
        let count = SETUP_CALLS.load(Ordering::SeqCst);
        anyhow::ensure!(
            count == 1,
            "setup must have run exactly once when both mods share \
             the same (runtime, suite, test) tuple; observed {count}",
        );
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
mod second_mod {
    use super::{CountingTest, SETUP_CALLS};
    use std::sync::atomic::Ordering;

    #[rudzio::test]
    fn in_second_mod(_ctx: &CountingTest) -> anyhow::Result<()> {
        let count = SETUP_CALLS.load(Ordering::SeqCst);
        anyhow::ensure!(
            count == 1,
            "setup must have run exactly once when both mods share \
             the same (runtime, suite, test) tuple; observed {count}",
        );
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
