//! Tokio `LocalRuntime` — a single-thread runtime that natively handles
//! `!Send` futures in `block_on`, `spawn_local`, and the framework runner.

use std::fmt;
use std::io;
use std::time::Duration;

use send_wrapper::SendWrapper;
use tokio::runtime::{Builder, LocalOptions, LocalRuntime};
use tokio::time::sleep;

use crate::config::Config;
use crate::runtime::tokio::error::tokio_join_error_to_join_error;
use crate::runtime::{JoinError, Runtime};

pub struct Local {
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
    /// Underlying tokio local runtime. `LocalRuntime` is `!Send`/`!Sync`;
    /// `SendWrapper` promotes this struct to `Send + Sync` so context types
    /// borrowing `&Local` can meet the framework's `Send + Sync` bounds.
    /// Access stays on the one group thread that owns the runtime.
    rt: SendWrapper<LocalRuntime>,
}

impl fmt::Debug for Local {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Local").finish_non_exhaustive()
    }
}

impl Local {
    /// Build a tokio `LocalRuntime`.
    ///
    /// Fields consulted from [`Config`]: **none** — this runtime is
    /// single-threaded by design. The full config is still stored and
    /// exposed via [`Runtime::config`](super::super::Runtime::config) so
    /// test bodies can read whatever they like.
    ///
    /// # Errors
    ///
    /// Returns an error if the tokio local runtime cannot be built.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let rt = Builder::new_current_thread()
            .enable_all()
            .build_local(LocalOptions::default())?;
        Ok(Self {
            rt: SendWrapper::new(rt),
            config: config.clone(),
        })
    }
}

impl<'rt> Runtime<'rt> for Local {
    #[inline]
    fn block_on<F>(&self, fut: F) -> anyhow::Result<F::Output>
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        Ok(self.rt.block_on(fut))
    }

    #[inline]
    fn config(&self) -> anyhow::Result<&Config> {
        Ok(&self.config)
    }

    #[inline]
    fn name(&self) -> anyhow::Result<&'static str> {
        Ok("tokio::Local")
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> anyhow::Result<impl Future<Output = ()> + Send + 'rt> {
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        // FIXME(rs-cleanup): callee returns Result, add ?
        Ok(sleep(duration))
    }

    #[inline]
    fn spawn<F>(&self, fut: F) -> anyhow::Result<impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt>
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        // `LocalRuntime` has no `spawn`; since it is single-threaded,
        // `spawn_local` is semantically equivalent and accepts `Send` futures.
        let handle = self.rt.spawn_local(fut);
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        Ok(async move { handle.await.map_err(tokio_join_error_to_join_error) })
    }

    #[inline]
    fn spawn_blocking<F, T>(
        &self,
        func: F,
    ) -> anyhow::Result<impl Future<Output = Result<T, JoinError>> + Send + 'rt>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.rt.spawn_blocking(func);
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        Ok(async move { handle.await.map_err(tokio_join_error_to_join_error) })
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> anyhow::Result<impl Future<Output = Result<F::Output, JoinError>> + 'rt>
    where
        F: Future + 'static,
    {
        let handle = self.rt.spawn_local(fut);
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        Ok(async move { handle.await.map_err(tokio_join_error_to_join_error) })
    }
}
