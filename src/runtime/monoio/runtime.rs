//! Monoio runtime wrapper implementing the [`Runtime`](crate::runtime::Runtime) trait.

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ::futures_util::FutureExt as _;
#[cfg(target_os = "linux")]
use ::monoio::IoUringDriver;
use ::monoio::LegacyDriver;
use ::monoio::time::TimeDriver;
use ::monoio::time::sleep as monoio_sleep;
use ::monoio::{FusionDriver, FusionRuntime, RuntimeBuilder, spawn as monoio_spawn};
use send_wrapper::SendWrapper;

use crate::config::Config;
use crate::runtime::JoinError;
use crate::runtime::Runtime as RuntimeTrait;

/// Concrete monoio runtime type produced by `RuntimeBuilder::<FusionDriver>::build`.
/// The shape is platform-conditional because monoio's `iouring` driver only
/// builds on linux; on macos the `FusionRuntime` collapses to a single legacy
/// branch.
#[cfg(target_os = "linux")]
type MonoioFusion = FusionRuntime<TimeDriver<IoUringDriver>, TimeDriver<LegacyDriver>>;

/// macos-side concrete monoio runtime type — legacy driver only (no iouring).
#[cfg(target_os = "macos")]
type MonoioFusion = FusionRuntime<TimeDriver<LegacyDriver>>;

pub struct Runtime {
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
    /// Underlying monoio runtime. Monoio's runtime is `!Send`/`!Sync` and
    /// `block_on` takes `&mut self`, so this lives behind a `RefCell` for
    /// interior mutation; `SendWrapper` then promotes the whole cell to
    /// `Send + Sync` for the framework's `DynRuntime` adapter (mirrors the
    /// compio adapter's pattern). Access always happens on the group thread.
    rt: SendWrapper<RefCell<MonoioFusion>>,
}

impl fmt::Debug for Runtime {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Build a monoio runtime with the timer driver enabled.
    ///
    /// Fields consulted from [`Config`]: **none** — monoio is single-threaded
    /// per instance (thread-per-core). The full config is still stored and
    /// exposed via [`RuntimeTrait::config`](crate::runtime::Runtime::config).
    ///
    /// # Errors
    ///
    /// Returns an error if the monoio runtime cannot be created (e.g. when
    /// the kernel does not support `io_uring` on linux).
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let rt = RuntimeBuilder::<FusionDriver>::new()
            .enable_timer()
            .build()?;
        Ok(Self {
            config: config.clone(),
            rt: SendWrapper::new(RefCell::new(rt)),
        })
    }
}

impl<'rt> RuntimeTrait<'rt> for Runtime {
    #[inline]
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        self.rt.borrow_mut().block_on(fut)
    }

    #[inline]
    fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    fn name(&self) -> &'static str {
        "monoio::Runtime"
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt {
        // monoio's timer is single-threaded; SendWrapper makes the future
        // Send while the runtime ensures it is always polled on the one
        // thread that owns the monoio runtime.
        SendWrapper::new(monoio_sleep(duration))
    }

    #[inline]
    fn spawn<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt
    where
        F: Future<Output: Send> + Send + 'static,
    {
        // monoio::spawn must be called inside the runtime's CURRENT scope,
        // which is set while block_on is polling. Test code reaches this
        // method through ctx.spawn, which is itself awaited under
        // block_on, so CURRENT is live here. Spawning eagerly mirrors the
        // compio adapter and side-steps clippy::manual_async_fn.
        let handle = monoio_spawn(AssertUnwindSafe(fut).catch_unwind());
        async move {
            match handle.await {
                Ok(value) => Ok(value),
                Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
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
        // monoio (built without the `sync` feature) has no native blocking
        // pool. Delegate to a freshly-spawned OS thread + std mpsc, mirroring
        // the embassy adapter's pattern.
        let (tx, rx) = mpsc::channel::<thread::Result<T>>();
        let _join: thread::JoinHandle<()> = thread::spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(func));
            let _send: Result<(), mpsc::SendError<thread::Result<T>>> = tx.send(result);
        });
        async move {
            match rx.recv() {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(payload)) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
                Err(err) => Err(JoinError::cancelled(err)),
            }
        }
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + 'rt
    where
        F: Future + 'static,
    {
        // monoio::spawn natively accepts !Send futures. Same eager-spawn
        // rationale as `spawn` above.
        let handle = monoio_spawn(AssertUnwindSafe(fut).catch_unwind());
        async move {
            match handle.await {
                Ok(value) => Ok(value),
                Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
            }
        }
    }
}

/// Convert a `catch_unwind` panic payload into an [`io::Error`] suitable for
/// [`JoinError::panicked`], preserving the original panic message when it's
/// a string.
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

/// Create a new monoio runtime instance.
///
/// # Errors
///
/// Returns an error if the monoio runtime cannot be created.
#[inline]
pub fn new(config: &Config) -> io::Result<Runtime> {
    Runtime::new(config)
}
