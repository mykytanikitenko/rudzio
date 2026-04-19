use std::any::Any;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use send_wrapper::SendWrapper;

use crate::runtime::{JoinError, Runtime};

pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Object-safe runtime abstraction used internally by the framework runner
/// and macro-generated helper functions.
///
/// The runner always references a `&'static dyn DynRuntime` across threads
/// and captures it inside spawned tasks, so `DynRuntime` must be
/// `Send + Sync + 'static`. A concrete [`Runtime`] implementation can
/// satisfy that either naturally (tokio multi-thread) or by internally
/// wrapping its thread-bound handles with `send_wrapper::SendWrapper`;
/// either way the blanket impl below picks it up automatically. For
/// runtimes that are structurally `!Send`/`!Sync`, [`SendAdapter`] provides
/// the same bridge from the outside.
#[doc(hidden)]
pub trait DynRuntime: Any + Send + Sync + 'static {
    /// Spawn a `Send + 'static` future onto the runtime, erasing its output
    /// type through `Box<dyn Any + Send>`.
    fn spawn_dyn(
        &self,
        fut: BoxFuture<Box<dyn Any + Send>>,
    ) -> BoxFuture<Result<Box<dyn Any + Send>, JoinError>>;

    /// Drive a type-erased future to completion on this runtime's thread.
    ///
    /// Called once per runtime group from its dedicated OS thread.
    fn block_on_erased(&self, fut: BoxFuture<Box<dyn Any + Send>>) -> Box<dyn Any + Send>;

    /// Returns a future that completes after `duration` using the runtime's
    /// native timer. Used by the runner for runtime-agnostic per-test timeouts.
    fn sleep_dyn(&self, duration: Duration) -> BoxFuture<()>;

    /// Yield control back to the runtime scheduler.
    fn yield_now_dyn(&self) -> BoxFuture<()>;

    /// Return a `&dyn Any` reference to the underlying concrete runtime so
    /// macro-generated helpers can `downcast_ref::<ConcreteRuntime>()` back
    /// to the original type and call its inherent API.
    fn as_any(&self) -> &dyn Any;
}

impl<T> DynRuntime for T
where
    T: Runtime<'static> + Send + Sync + 'static,
{
    #[inline]
    fn spawn_dyn(
        &self,
        fut: BoxFuture<Box<dyn Any + Send>>,
    ) -> BoxFuture<Result<Box<dyn Any + Send>, JoinError>> {
        Box::pin(<Self as Runtime<'static>>::spawn(self, fut))
    }

    #[inline]
    fn block_on_erased(&self, fut: BoxFuture<Box<dyn Any + Send>>) -> Box<dyn Any + Send> {
        <Self as Runtime<'static>>::block_on(self, fut)
    }

    #[inline]
    fn sleep_dyn(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(<Self as Runtime<'static>>::sleep(self, duration))
    }

    #[inline]
    fn yield_now_dyn(&self) -> BoxFuture<()> {
        let mut yielded = false;
        Box::pin(poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }

    #[inline]
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Adapter that promotes an otherwise `!Send`/`!Sync` [`Runtime<'static>`] to
/// a `Send + Sync` [`DynRuntime`] by wrapping it in `SendWrapper`.
///
/// Use when a runtime cannot (or should not) be `Send + Sync` on its own but
/// the framework still needs a cross-thread handle. `SendWrapper`'s
/// thread-locality check enforces — at runtime — the single-thread
/// invariant the framework already maintains.
#[doc(hidden)]
#[derive(Debug)]
pub struct SendAdapter<R> {
    inner: SendWrapper<R>,
}

impl<R> SendAdapter<R> {
    #[inline]
    pub fn new(runtime: R) -> Self {
        Self {
            inner: SendWrapper::new(runtime),
        }
    }
}

impl<R> DynRuntime for SendAdapter<R>
where
    R: Runtime<'static> + 'static,
{
    #[inline]
    fn spawn_dyn(
        &self,
        fut: BoxFuture<Box<dyn Any + Send>>,
    ) -> BoxFuture<Result<Box<dyn Any + Send>, JoinError>> {
        Box::pin(<R as Runtime<'static>>::spawn(&self.inner, fut))
    }

    #[inline]
    fn block_on_erased(&self, fut: BoxFuture<Box<dyn Any + Send>>) -> Box<dyn Any + Send> {
        <R as Runtime<'static>>::block_on(&self.inner, fut)
    }

    #[inline]
    fn sleep_dyn(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(<R as Runtime<'static>>::sleep(&self.inner, duration))
    }

    #[inline]
    fn yield_now_dyn(&self) -> BoxFuture<()> {
        let mut yielded = false;
        Box::pin(poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }

    #[inline]
    fn as_any(&self) -> &dyn Any {
        &*self.inner
    }
}
