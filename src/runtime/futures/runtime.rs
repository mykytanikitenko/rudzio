//! `futures::executor::ThreadPool` runtime implementation.

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::io;
use std::time::Duration;

use ::futures_executor::{
    LocalPool, LocalSpawner, ThreadPool as FuturesThreadPool, ThreadPoolBuilder,
};
use ::futures_timer::Delay;
use ::futures_util::FutureExt;
use ::futures_util::task::{LocalSpawnExt as _, SpawnExt as _};
use send_wrapper::SendWrapper;

use crate::config::Config;
use crate::runtime::{JoinError, Runtime};

pub struct ThreadPool {
    /// Shared Send thread pool used by `spawn` / `spawn_blocking`.
    pool: FuturesThreadPool,
    /// Per-runtime `LocalPool` driving `spawn_local` tasks. `!Send`, so
    /// wrapped in `SendWrapper` — only ever touched on the group thread
    /// that owns the runtime.
    local: SendWrapper<RefCell<LocalPool>>,
    /// Cached handle used to enqueue `!Send` futures onto `local`.
    local_spawner: SendWrapper<LocalSpawner>,
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
}

impl fmt::Debug for ThreadPool {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadPool").finish_non_exhaustive()
    }
}

impl ThreadPool {
    /// Build a `futures::executor::ThreadPool`-backed runtime.
    ///
    /// Fields consulted from [`Config`]:
    /// - [`Config::threads`] → `ThreadPoolBuilder::pool_size`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying `ThreadPool` cannot be built.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let pool = ThreadPoolBuilder::new()
            .pool_size(config.threads)
            .create()
            .map_err(io::Error::other)?;
        let local = LocalPool::new();
        let local_spawner = local.spawner();
        Ok(Self {
            pool,
            local: SendWrapper::new(RefCell::new(local)),
            local_spawner: SendWrapper::new(local_spawner),
            config: config.clone(),
        })
    }
}

impl<'rt> Runtime<'rt> for ThreadPool {
    #[inline]
    fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    fn name(&self) -> &'static str {
        "futures::ThreadPool"
    }

    #[inline]
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        // `run_until` drives every queued LocalPool task alongside `fut`,
        // so `spawn_local` futures make progress for free.
        self.local.borrow_mut().run_until(fut)
    }

    #[inline]
    fn spawn<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        let handle = self
            .pool
            .spawn_with_handle(std::panic::AssertUnwindSafe(fut).catch_unwind());
        async move {
            match handle {
                Ok(remote) => match remote.await {
                    Ok(value) => Ok(value),
                    Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
                },
                Err(err) => Err(JoinError::cancelled(io::Error::other(err.to_string()))),
            }
        }
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
        // No dedicated blocking pool; delegate to the main ThreadPool. Heavy
        // blocking workloads can starve executor threads — pick a different
        // runtime for those.
        let handle = self.pool.spawn_with_handle(async move {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(func))
        });
        async move {
            match handle {
                Ok(remote) => match remote.await {
                    Ok(value) => Ok(value),
                    Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
                },
                Err(err) => Err(JoinError::cancelled(io::Error::other(err.to_string()))),
            }
        }
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + 'rt
    where
        F: Future + 'static,
    {
        let handle = self
            .local_spawner
            .spawn_local_with_handle(std::panic::AssertUnwindSafe(fut).catch_unwind());
        async move {
            match handle {
                Ok(remote) => match remote.await {
                    Ok(value) => Ok(value),
                    Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
                },
                Err(err) => Err(JoinError::cancelled(io::Error::other(err.to_string()))),
            }
        }
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt {
        Delay::new(duration)
    }
}

/// Map a `catch_unwind` panic payload to [`JoinError::Panicked`]'s underlying
/// `io::Error`, preserving the original panic message when it's a string.
#[inline]
fn panic_payload_to_io_error(payload: &(dyn Any + Send)) -> io::Error {
    let message = payload.downcast_ref::<&'static str>().map_or_else(
        || {
            payload.downcast_ref::<String>().map_or_else(
                || "task panicked".to_owned(),
                |msg| format!("task panicked: {msg}"),
            )
        },
        |msg| format!("task panicked: {msg}"),
    );
    io::Error::other(message)
}
