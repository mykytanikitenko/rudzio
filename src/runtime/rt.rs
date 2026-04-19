use std::future::poll_fn;
use std::task::Poll;
use std::time::Duration;

use crate::runtime::JoinError;

/// Async runtime abstraction.
///
/// Runtime implementations are not required to be `Send + Sync`. The framework
/// only ever uses a runtime from the single OS thread that created it, so
/// per-runtime structs can freely embed thread-bound primitives (e.g. compio's
/// own runtime handle, embassy's `Spawner`) without extra wrapping. The
/// `Send + Sync` assertion happens one level up, inside the `DynRuntime`
/// adapter that the runner actually holds.
pub trait Runtime<'rt>: 'rt {
    /// Block the current thread until `fut` completes.
    ///
    /// `fut` is not required to be `Send`: `block_on` drives it on the
    /// calling thread and never hands it off. Runtimes whose underlying
    /// `block_on` primitive does demand `Send` are expected to wrap the
    /// future internally with `send_wrapper::SendWrapper`.
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'rt,
        F::Output: 'static;

    /// Spawn a `Send` future onto the runtime.
    fn spawn<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt
    where
        F: Future + Send + 'static,
        F::Output: Send;

    /// Spawn a blocking closure onto a thread suitable for blocking I/O.
    fn spawn_blocking<F, T>(
        &self,
        func: F,
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'rt
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static;

    /// Spawn a `!Send` future onto a thread-local executor.
    ///
    /// Runtimes that can only drive `Send` futures are expected to emulate
    /// local execution by wrapping `fut` (and its output where needed) with
    /// `send_wrapper::SendWrapper`.
    fn spawn_local<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + 'rt
    where
        F: Future + 'static;

    /// Yield control back to the runtime scheduler.
    ///
    /// Default implementation is runtime-agnostic: return `Pending` once after
    /// re-scheduling the current task, then complete on the next poll.
    #[inline]
    fn yield_now(&self) -> impl Future<Output = ()> + 'rt {
        let mut yielded = false;
        poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    /// Sleep for the given `duration` using the runtime's native timer.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt;
}
