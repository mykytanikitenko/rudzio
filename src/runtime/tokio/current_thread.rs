//! Current-thread tokio runtime paired with a `LocalSet` for `!Send` futures.

use std::fmt;
use std::io;
use std::time::Duration;

use send_wrapper::SendWrapper;
use tokio::runtime::{Builder, Runtime as TokioRuntime};
use tokio::task::LocalSet;
use tokio::time::sleep;

use crate::config::Config;
use crate::runtime::tokio::error::tokio_join_error_to_join_error;
use crate::runtime::{JoinError, Runtime};

pub struct CurrentThread {
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
    /// Thread-local executor used to drive `!Send` futures. `LocalSet` is
    /// `!Send`/`!Sync`; `SendWrapper` promotes this struct to `Send + Sync`
    /// so context types borrowing `&CurrentThread` can meet the framework's
    /// `Send + Sync` bounds. The runtime is only ever touched from its
    /// owning group thread, so `SendWrapper`'s thread-locality check does
    /// not trigger in practice.
    local_set: SendWrapper<LocalSet>,
    /// Underlying tokio current-thread runtime.
    rt: TokioRuntime,
}

impl fmt::Debug for CurrentThread {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CurrentThread").finish_non_exhaustive()
    }
}

impl CurrentThread {
    /// Build a current-thread tokio runtime.
    ///
    /// Fields consulted from [`Config`]: **none** — this runtime is
    /// single-threaded by design, so `config.threads` would be misleading
    /// here. The full config is still stored and exposed via
    /// [`Runtime::config`](super::super::Runtime::config) so test bodies
    /// can read whatever they like (e.g. `config.env`, `config.unparsed`).
    ///
    /// # Errors
    ///
    /// Returns an error if the tokio runtime cannot be built.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let rt = Builder::new_current_thread().enable_all().build()?;
        let local_set = SendWrapper::new(LocalSet::new());
        Ok(Self {
            local_set,
            rt,
            config: config.clone(),
        })
    }
}

impl<'rt> Runtime<'rt> for CurrentThread {
    #[inline]
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        self.local_set.block_on(&self.rt, fut)
    }

    #[inline]
    fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    fn name(&self) -> &'static str {
        "tokio::CurrentThread"
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt {
        sleep(duration)
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
        let handle = self.local_set.spawn_local(fut);
        async move { handle.await.map_err(tokio_join_error_to_join_error) }
    }
}
