//! `async-std` runtime implementation.

use std::any::Any;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

use ::async_std::task;
use ::futures_util::FutureExt as _;

use crate::config::Config;
use crate::runtime::{JoinError, Runtime as RuntimeTrait};

/// `async-std`-backed runtime adapter.
pub struct Runtime {
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
    /// Build an `async-std`-backed runtime.
    ///
    /// `async-std` exposes a global executor with no per-instance handle, so
    /// the runtime struct only retains the resolved [`Config`] for the
    /// `Runtime::config()` accessor.
    ///
    /// Fields consulted from [`Config`]: none (`config.threads` is ignored
    /// because `async-std` provides no public knob to size its global pool).
    ///
    /// # Errors
    ///
    /// Currently infallible; the `io::Result` return matches the constructor
    /// shape used by every other runtime adapter so the suite-codegen can call
    /// each one uniformly.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        Ok(Self {
            config: config.clone(),
        })
    }
}

impl<'rt> RuntimeTrait<'rt> for Runtime {
    #[inline]
    fn block_on<F>(&self, fut: F) -> anyhow::Result<F::Output>
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        // FIXME(rs-cleanup): callee returns Result, add ?
        Ok(task::block_on(fut))
    }

    #[inline]
    fn config(&self) -> anyhow::Result<&Config> {
        Ok(&self.config)
    }

    #[inline]
    fn name(&self) -> anyhow::Result<&'static str> {
        Ok("async_std::Runtime")
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> anyhow::Result<impl Future<Output = ()> + Send + 'rt> {
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        // FIXME(rs-cleanup): callee returns Result, add ?
        Ok(task::sleep(duration))
    }

    #[inline]
    fn spawn<F>(&self, fut: F) -> anyhow::Result<impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt>
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        // FIXME(rs-cleanup): callee returns Result, add ?
        let handle = task::spawn(AssertUnwindSafe(fut).catch_unwind());
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        Ok(async move {
            match handle.await {
                Ok(value) => Ok(value),
                // FIXME(rs-cleanup): callee returns Result, add ?
                Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
            }
        })
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
        // FIXME(rs-cleanup): callee returns Result, add ?
        let handle = task::spawn_blocking(move || panic::catch_unwind(AssertUnwindSafe(func)));
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        Ok(async move {
            match handle.await {
                Ok(value) => Ok(value),
                // FIXME(rs-cleanup): callee returns Result, add ?
                Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
            }
        })
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> anyhow::Result<impl Future<Output = Result<F::Output, JoinError>> + 'rt>
    where
        F: Future + 'static,
    {
        // FIXME(rs-cleanup): callee returns Result, add ?
        let handle = task::spawn_local(AssertUnwindSafe(fut).catch_unwind());
        // FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles
        Ok(async move {
            match handle.await {
                Ok(value) => Ok(value),
                // FIXME(rs-cleanup): callee returns Result, add ?
                Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
            }
        })
    }
}

/// Map a `catch_unwind` panic payload to [`JoinError::Panicked`]'s underlying
/// `io::Error`, preserving the original panic message when it's a string.
#[inline]
fn panic_payload_to_io_error(payload: &(dyn Any + Send)) -> anyhow::Result<io::Error> {
    let message = payload.downcast_ref::<&'static str>().map_or_else(
        || {
            payload.downcast_ref::<String>().map_or_else(
                || "task panicked".to_owned(),
                |msg| format!("task panicked: {msg}"),
            )
        },
        |msg| format!("task panicked: {msg}"),
    );
    Ok(io::Error::other(message))
}
