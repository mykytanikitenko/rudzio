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
use rudzio::runtime::tokio::Multithread;
use rudzio::tokio_util::sync::CancellationToken;

/// Suite context with no shared state beyond a runtime borrow.
struct MutableSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'suite_context R>,
}

/// Per-test context exposing a private `u32` counter mutated by the test
/// body via `&mut self` accessors. The fixture verifies the suite macro
/// preserves the `mut` qualifier when binding the per-test context.
struct MutableTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Ties the struct to the runtime lifetime without carrying any state.
    _marker: PhantomData<&'test_context R>,
    /// Counter mutated by the test body; reset to 0 by every fresh
    /// `Suite::context` invocation so per-test isolation can be asserted.
    counter: u32,
}

impl<'suite_context, R> fmt::Debug for MutableSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MutableSuite").finish_non_exhaustive()
    }
}

impl<'test_context, R> fmt::Debug for MutableTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MutableTest")
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for MutableSuite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{
    type ContextError = Infallible;
    type SetupError = Infallible;
    type TeardownError = Infallible;
    type Test<'test_context>
        = MutableTest<'test_context, R>
    where
        Self: 'test_context;

    #[inline]
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

    #[inline]
    async fn setup(
        _rt: &'suite_context R,
        _cancel: CancellationToken,
        _config: &'suite_context ::rudzio::Config,
    ) -> Result<Self, Self::SetupError> {
        Ok(Self {
            _marker: PhantomData,
        })
    }

    #[inline]
    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

impl<'test_context, R> MutableTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Read the current counter value (used by test bodies to assert
    /// isolation and the result of mutations).
    const fn counter(&self) -> u32 {
        self.counter
    }

    /// Increment the counter by 1 using saturating arithmetic.
    const fn increment(&mut self) {
        self.counter = self.counter.saturating_add(1);
    }

    /// Replace the counter with the given value.
    const fn set_counter(&mut self, value: u32) {
        self.counter = value;
    }
}

impl<'test_context, R> context::Test<'test_context, R> for MutableTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    #[inline]
    async fn teardown(
        self,
        _cancel: CancellationToken,
    ) -> Result<(), Self::TeardownError> {
        Ok(())
    }
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = MutableSuite,
        test = MutableTest,
    ),
])]
mod tests {
    use super::MutableTest;

    /// Mutate the per-test counter via `&mut` and assert the new value.
    /// Each test gets a fresh ctx, so this should always read 0 -> 3.
    #[rudzio::test]
    async fn mutates_via_mut_borrow(ctx: &mut MutableTest) -> anyhow::Result<()> {
        anyhow::ensure!(ctx.counter() == 0, "fresh ctx should start at 0");
        ctx.increment();
        ctx.increment();
        ctx.increment();
        anyhow::ensure!(ctx.counter() == 3, "counter must reflect mutations");
        Ok(())
    }

    /// Sync test body still gets `&mut` access.
    #[rudzio::test]
    fn sync_mutates_via_mut_borrow(ctx: &mut MutableTest) -> anyhow::Result<()> {
        ctx.set_counter(42);
        anyhow::ensure!(ctx.counter() == 42, "sync &mut must work too");
        Ok(())
    }

    /// Verify isolation — a new ctx is created per test, so the counter is
    /// 0 here even though the previous tests left their ctx with non-zero
    /// values.
    #[rudzio::test]
    async fn fresh_ctx_per_test(ctx: &mut MutableTest) -> anyhow::Result<()> {
        anyhow::ensure!(
            ctx.counter() == 0,
            "each test should get a fresh ctx — got counter={}",
            ctx.counter(),
        );
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
