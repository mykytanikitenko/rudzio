//! Multi-thread tokio runtime implementation.

use std::fmt;
use std::io;
use std::time::Duration;

use send_wrapper::SendWrapper;
use tokio::runtime::{Builder, Runtime as TokioRuntime};
use tokio::time::sleep;

use crate::runtime::tokio::error::tokio_join_error_to_join_error;
use crate::runtime::{JoinError, Runtime};

pub struct Multithread {
    /// Underlying tokio multi-thread runtime.
    rt: TokioRuntime,
}

impl fmt::Debug for Multithread {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Multithread").finish_non_exhaustive()
    }
}

impl Multithread {
    /// # Errors
    ///
    /// Returns an error if the tokio runtime cannot be built.
    #[inline]
    pub fn new() -> io::Result<Self> {
        let rt = Builder::new_multi_thread().enable_all().build()?;
        Ok(Self { rt })
    }
}

impl<'rt> Runtime<'rt> for Multithread {
    #[inline]
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        self.rt.block_on(fut)
    }

    #[inline]
    fn spawn<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        let handle = self.rt.spawn(fut);
        async move { handle.await.map_err(tokio_join_error_to_join_error) }
    }

    #[inline]
    fn spawn_blocking<F, T>(
        &self,
        func: F,
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'rt
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.rt.spawn_blocking(func);
        async move { handle.await.map_err(tokio_join_error_to_join_error) }
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + 'rt
    where
        F: Future + 'static,
    {
        // The multi-thread tokio runtime only schedules `Send` futures, so
        // wrap both the input future and its output with `SendWrapper` so the
        // underlying `spawn` call is satisfied. Acceptable for the test
        // framework: `spawn_local` is only used for `Rc`/`RefCell`-like values
        // and will panic if touched from a thread other than the one that
        // produced them — which surfaces as a regular test failure.
        let wrapped_fut = SendWrapper::new(async move { SendWrapper::new(fut.await) });
        let handle = self.rt.spawn(wrapped_fut);
        async move {
            handle
                .await
                .map(SendWrapper::take)
                .map_err(tokio_join_error_to_join_error)
        }
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt {
        sleep(duration)
    }
}
