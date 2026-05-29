//! `smol` runtime implementation.

use std::any::Any;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ::futures_util::FutureExt as _;
use ::smol::channel::{self, Receiver, RecvError, Sender};
use ::smol::{Executor, LocalExecutor, Timer, unblock};
use send_wrapper::SendWrapper;

use crate::config::Config;
use crate::runtime::{JoinError, Runtime as RuntimeTrait};

/// `smol`-backed runtime adapter.
///
/// Owns a shared [`Executor`] driven by `config.threads` worker OS threads
/// and a per-runtime [`LocalExecutor`] for `!Send` futures. Workers run
/// `smol::block_on(executor.run(shutdown_rx.recv()))` and exit on
/// [`Drop`](Self::drop) when the shutdown sender is closed.
pub struct Runtime {
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
    /// Shared `Send` executor; cloned into each worker thread.
    executor: Arc<Executor<'static>>,
    /// Per-runtime `!Send` executor for `spawn_local`. Wrapped because the
    /// framework requires `Send + Sync` runtimes; the wrapper's thread check
    /// never trips because all access happens on the group thread.
    local: SendWrapper<LocalExecutor<'static>>,
    /// Shutdown sender held by the runtime; dropping it on
    /// [`Drop`](Self::drop) closes the channel and lets each worker future
    /// resolve so `executor.run(_)` returns.
    shutdown: Option<Sender<()>>,
    /// Worker thread join handles; `Option` so [`Drop`](Self::drop) can take
    /// them out one-by-one and join.
    workers: Vec<Option<thread::JoinHandle<()>>>,
}

impl fmt::Debug for Runtime {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Build a `smol`-backed runtime.
    ///
    /// Spawns `config.threads` OS threads, each pumping the shared
    /// [`Executor`] until the shutdown channel closes on
    /// [`Drop`](Self::drop). The local executor is created up front and
    /// wrapped in [`SendWrapper`].
    ///
    /// Fields consulted from [`Config`]:
    /// - [`Config::threads`] → number of worker OS threads.
    ///
    /// # Errors
    ///
    /// Returns an error if a worker [`thread::Builder::spawn`] call fails.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let executor: Arc<Executor<'static>> = Arc::new(Executor::new());
        let (shutdown_tx, shutdown_rx) = channel::bounded::<()>(1);
        let mut workers: Vec<Option<thread::JoinHandle<()>>> = Vec::with_capacity(config.threads);
        for index in 0..config.threads {
            let executor_for_worker = Arc::clone(&executor);
            let shutdown_for_worker = shutdown_rx.clone();
            let handle = thread::Builder::new()
                .name(format!("rudzio-smol-worker-{index}"))
                .spawn(move || {
                    drive_worker(&executor_for_worker, shutdown_for_worker);
                })?;
            workers.push(Some(handle));
        }
        Ok(Self {
            config: config.clone(),
            executor,
            local: SendWrapper::new(LocalExecutor::new()),
            shutdown: Some(shutdown_tx),
            workers,
        })
    }
}

impl Drop for Runtime {
    #[inline]
    fn drop(&mut self) {
        // Closing the shutdown sender lets every worker's `recv().await`
        // resolve, which in turn lets `executor.run(_)` return so the
        // worker thread exits.
        drop(self.shutdown.take());
        for slot in &mut self.workers {
            if let Some(handle) = slot.take() {
                match handle.join() {
                    Ok(()) => {}
                    Err(_payload) => {
                        // A worker thread panicked. There is no caller-facing
                        // path to surface this from `Drop`; ignore.
                    }
                }
            }
        }
    }
}

impl<'rt> RuntimeTrait<'rt> for Runtime {
    #[inline]
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        // `local.run(fut)` drives any pending `spawn_local` tasks alongside
        // `fut`; the global executor is driven concurrently by worker threads.
        ::smol::block_on(self.local.run(fut))
    }

    #[inline]
    fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    fn name(&self) -> &'static str {
        "smol::Runtime"
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt {
        Timer::after(duration).map(drop)
    }

    #[inline]
    fn spawn<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        let task = self.executor.spawn(AssertUnwindSafe(fut).catch_unwind());
        async move {
            match task.await {
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
        let task = unblock(move || panic::catch_unwind(AssertUnwindSafe(func)));
        async move {
            match task.await {
                Ok(value) => Ok(value),
                Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
            }
        }
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + 'rt
    where
        F: Future + 'static,
    {
        let task = self.local.spawn(AssertUnwindSafe(fut).catch_unwind());
        async move {
            match task.await {
                Ok(value) => Ok(value),
                Err(payload) => Err(JoinError::panicked(panic_payload_to_io_error(&*payload))),
            }
        }
    }
}

/// Run a single worker thread: pump `executor` until the shutdown channel
/// closes, then return so the OS thread exits cleanly.
fn drive_worker(executor: &Executor<'static>, shutdown: Receiver<()>) {
    ::smol::block_on(executor.run(async move {
        // `recv()` returns `Err` once the sender is dropped; either outcome
        // ends the worker future.
        let _ignored: Result<(), RecvError> = shutdown.recv().await;
    }));
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
