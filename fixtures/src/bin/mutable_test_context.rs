//! Verifies the suite macro supports `&mut TestCtx` as the test parameter.
//!
//! The `MutableTest` context carries a plain `u32` counter (no interior
//! mutability). A `&mut MutableTest` test mutates it and asserts the new
//! value, which only compiles + runs correctly if the macro:
//!   1. transforms `&mut MutableTest` → `&'a mut MutableTest<'a, R>` while
//!      preserving the `mut` qualifier;
//!   2. binds the per-test ctx as `let mut ctx = ...` so `&mut ctx` is
//!      borrowable;
//!   3. dispatches with `&mut ctx`, not `&ctx`.

use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;

use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::tokio_util::sync::CancellationToken;

pub struct MutableSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    _marker: PhantomData<&'suite_context R>,
}

impl<'suite_context, R> fmt::Debug for MutableSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MutableSuite").finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for MutableSuite<'suite_context, R>
where
    R: for<'r> Runtime<'r> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = MutableTest<'test_context, R>
    where
        Self: 'test_context;

    async fn context<'test_context>(
        &'test_context self,
        _cancel: CancellationToken,
        _config: &'test_context ::rudzio::Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        Ok(MutableTest {
            _marker: PhantomData,
            counter: 0,
        })
    }

    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
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

pub struct MutableTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    _marker: PhantomData<&'test_context R>,
    pub counter: u32,
}

impl<'test_context, R> fmt::Debug for MutableTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MutableTest")
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for MutableTest<'test_context, R>
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
        suite = MutableSuite,
        test = MutableTest,
    ),
])]
mod tests {
    use super::MutableTest;

    /// Mutate the per-test counter via `&mut` and assert the new value.
    /// Each test gets a fresh ctx, so this should always read 0 → 3.
    #[rudzio::test]
    async fn mutates_via_mut_borrow(ctx: &mut MutableTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.counter == 0, "fresh ctx should start at 0");
        ctx.counter += 1;
        ctx.counter += 1;
        ctx.counter += 1;
        anyhow::ensure!(ctx.counter == 3, "counter must reflect mutations");
        Ok(())
    }

    /// Sync test body still gets `&mut` access.
    #[rudzio::test]
    fn sync_mutates_via_mut_borrow(ctx: &mut MutableTest) -> anyhow::Result<()> {
        ctx.counter = 42;
        anyhow::ensure!(ctx.counter == 42, "sync &mut must work too");
        Ok(())
    }

    /// Verify isolation — a new ctx is created per test, so the counter is
    /// 0 here even though the previous tests left their ctx with non-zero
    /// values.
    #[rudzio::test]
    async fn fresh_ctx_per_test(ctx: &mut MutableTest) -> anyhow::Result<()> {
        anyhow::ensure!(
            ctx.counter == 0,
            "each test should get a fresh ctx — got counter={}",
            ctx.counter,
        );
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
