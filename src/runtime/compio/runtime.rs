//! Compio runtime wrapper implementing the [`Runtime`](crate::runtime::Runtime) trait.

use std::any::Any;
use std::fmt;
use std::io;
use std::time::Duration;

use send_wrapper::SendWrapper;

use crate::config::Config;
use crate::runtime::JoinError;
use crate::runtime::Runtime as RuntimeTrait;

pub struct Runtime {
    /// Underlying compio runtime. `compio_runtime::Runtime` is `!Send`/`!Sync`;
    /// `SendWrapper` promotes it to `Send + Sync` so user context types that
    /// borrow the runtime (e.g. `rudzio::common::context::Suite<R>`) can themselves
    /// satisfy the framework's `Send + Sync` bound on context traits. Access
    /// always happens on the group thread, so `SendWrapper`'s thread-locality
    /// check is never triggered in practice.
    rt: SendWrapper<::compio_runtime::Runtime>,
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
}

impl fmt::Debug for Runtime {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Build a compio runtime.
    ///
    /// Fields consulted from [`Config`]: **none** — compio's io_uring
    /// driver is single-threaded. The full config is still stored and
    /// exposed via [`RuntimeTrait::config`](crate::runtime::Runtime::config).
    ///
    /// # Errors
    ///
    /// Returns an error if the compio runtime cannot be created.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let rt = ::compio_runtime::Runtime::new()?;
        Ok(Self {
            rt: SendWrapper::new(rt),
            config: config.clone(),
        })
    }
}

impl<'rt> RuntimeTrait<'rt> for Runtime {
    #[inline]
    fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    fn name(&self) -> &'static str {
        "compio::Runtime"
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
        F: Future<Output: Send> + Send + 'static,
    {
        let handle = self.rt.spawn(fut);
        async move {
            match handle.await {
                Ok(result) => Ok(result),
                Err(payload) => Err(compio_join_error_to_join_error(payload.as_ref())),
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
        let handle = self.rt.spawn_blocking(func);
        async move {
            match handle.await {
                Ok(result) => Ok(result),
                Err(payload) => Err(compio_join_error_to_join_error(payload.as_ref())),
            }
        }
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + 'rt
    where
        F: Future + 'static,
    {
        // compio's `spawn` natively accepts `!Send` futures.
        let handle = self.rt.spawn(fut);
        async move {
            match handle.await {
                Ok(result) => Ok(result),
                Err(payload) => Err(compio_join_error_to_join_error(payload.as_ref())),
            }
        }
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt {
        // compio's timer is single-threaded; `SendWrapper` makes the future
        // `Send` while the runtime ensures it is always polled on the one
        // thread that owns the compio runtime.
        SendWrapper::new(::compio_runtime::time::sleep(duration))
    }
}

/// Convert a compio panic payload into a [`JoinError::Panicked`].
#[inline]
fn compio_join_error_to_join_error(payload: &(dyn Any + Send)) -> JoinError {
    let message = payload.downcast_ref::<&'static str>().map_or_else(
        || {
            payload.downcast_ref::<String>().map_or_else(
                || "task panicked".to_owned(),
                |msg| format!("task panicked: {msg}"),
            )
        },
        |msg| format!("task panicked: {msg}"),
    );
    JoinError::panicked(io::Error::other(message))
}

/// Create a new compio runtime instance.
///
/// # Errors
///
/// Returns an error if the compio runtime cannot be created.
#[inline]
pub fn new(config: &Config) -> io::Result<Runtime> {
    Runtime::new(config)
}
