//! Multi-thread tokio runtime implementation.

use std::fmt;
use std::io;
use std::time::Duration;

use send_wrapper::SendWrapper;
use tokio::runtime::{Builder, Runtime as TokioRuntime};
use tokio::time::sleep;

use crate::config::Config;
use crate::runtime::tokio::error::tokio_join_error_to_join_error;
use crate::runtime::{JoinError, Runtime};

pub struct Multithread {
    /// Underlying tokio multi-thread runtime.
    rt: TokioRuntime,
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
}

impl fmt::Debug for Multithread {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Multithread").finish_non_exhaustive()
    }
}

impl Multithread {
    /// Build a tokio multi-thread runtime.
    ///
    /// Fields consulted from [`Config`]:
    /// - [`Config::threads`] → `tokio::runtime::Builder::worker_threads`.
    ///
    /// Other fields (`concurrency_limit`, `test_timeout`, `run_timeout`,
    /// `env`, `unparsed`, …) are retained via [`Runtime::config`] for the
    /// test body to inspect but are not consumed here. To surface
    /// additional tokio knobs (`max_blocking_threads`, `thread_stack_size`,
    /// etc.), write a custom constructor that reads `config.env` /
    /// `config.unparsed` and call `Builder` directly.
    ///
    /// # Errors
    ///
    /// Returns an error if the tokio runtime cannot be built.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let rt = Builder::new_multi_thread()
            .worker_threads(config.threads)
            .enable_all()
            .build()?;
        Ok(Self {
            rt,
            config: config.clone(),
        })
    }
}

impl<'rt> Runtime<'rt> for Multithread {
    #[inline]
    fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    fn name(&self) -> &'static str {
        "tokio::Multithread"
    }

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
