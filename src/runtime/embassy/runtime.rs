//! Embassy runtime built directly on `embassy_executor::raw::Executor`.
//!
//! The runtime lives on the caller's group thread. `block_on` drives the
//! executor in-place with a `poll + park` loop until the target task writes
//! its output back into a caller-owned slot. No background executor thread,
//! no cross-thread channels on the block-on path, no `ForceSend`-style
//! wrappers.

#![allow(
    unsafe_code,
    reason = "unsafe glue around embassy_executor::raw::Executor — see per-site SAFETY comments"
)]

use std::fmt;
use std::future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::ptr;
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};
use std::task::Poll;
use std::thread;
use std::time::Duration;

use embassy_executor::Spawner;
use embassy_executor::raw::{Executor, TaskStorage};
use send_wrapper::SendWrapper;

use crate::config::Config;
use crate::runtime::{JoinError, Runtime as RuntimeTrait};

/// The pender callback embassy-executor invokes when a task becomes ready.
///
/// Required by `embassy_executor::raw::Executor`; must be an exported symbol
/// named `__pender`. We route the signal to the `Signaler` associated with
/// the executor via the `context` pointer that `Executor::new` stored.
#[unsafe(export_name = "__pender")]
fn __pender(context: *mut ()) {
    // SAFETY: `context` is the `&'static Signaler` pointer we passed to
    // `Executor::new` in `Runtime::new`; it lives for the rest of the
    // process.
    let signaler: &'static Signaler = unsafe { &*(context.cast::<Signaler>()) };
    signaler.signal();
}

/// Condvar-backed parking primitive that matches `embassy_executor::arch::std`.
///
/// The executor's pender calls `signal()` from arbitrary threads (e.g. a wake
/// issued from a timer thread inside `sleep`). `block_on` calls `wait()` on
/// the group thread to sleep until that happens.
struct Signaler {
    /// Condvar woken by `signal()` to release a parked `wait()` caller.
    condvar: Condvar,
    /// Latch set by `signal()` so a `wait()` racing against `signal()`
    /// observes the wake.
    flag: Mutex<bool>,
}

impl Signaler {
    /// Build a signaler with the latch cleared and no waiters.
    const fn new() -> Self {
        Self {
            flag: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    /// Set the latch and wake any thread parked in [`Self::wait`].
    fn signal(&self) {
        let mut guard = self.flag.lock().expect("signaler flag poisoned");
        *guard = true;
        self.condvar.notify_one();
    }

    /// Block until [`Self::signal`] is observed, clearing the latch
    /// before returning so the next wait can be re-armed.
    fn wait(&self) {
        let mut guard = self.flag.lock().expect("signaler flag poisoned");
        while !*guard {
            guard = self.condvar.wait(guard).expect("signaler condvar poisoned");
        }
        *guard = false;
    }
}

/// Type-erased future boxed so one `TaskStorage` concrete type covers every
/// spawn. `!Send` is fine because embassy tasks run on the executor thread.
type ErasedTask = Pin<Box<dyn Future<Output = ()> + 'static>>;

pub struct Runtime {
    /// Resolved [`Config`] this runtime was constructed from.
    config: Config,
    /// Raw executor leaked to `'static` so it can hand out `Spawner`s.
    ///
    /// `Executor` contains a `PhantomData<*mut ()>` which marks it `!Sync`;
    /// `SendWrapper` promotes the whole runtime to `Send + Sync`, matching
    /// what user context types borrowing `&Runtime` need.
    executor: SendWrapper<&'static Executor>,
    /// Shared signaler driving the `block_on` parking loop.
    signaler: &'static Signaler,
    /// Cached spawner. Also `!Send` via a `*mut ()` `PhantomData`.
    spawner: SendWrapper<Spawner>,
}

impl fmt::Debug for Runtime {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Drive the executor until `done` returns `Some`, then return its value.
    fn drive_until<T>(&self, mut done: impl FnMut() -> Option<T>) -> T {
        loop {
            if let Some(value) = done() {
                return value;
            }
            // SAFETY: the executor was initialized by `Runtime::new` and we
            // never re-enter `poll` (this loop is the sole caller).
            unsafe {
                self.executor.poll();
            }
            if let Some(value) = done() {
                return value;
            }
            self.signaler.wait();
        }
    }

    /// Build an embassy runtime.
    ///
    /// Fields consulted from [`Config`]: **none** — embassy's executor is
    /// single-threaded. The full config is still stored and exposed via
    /// [`RuntimeTrait::config`](crate::runtime::Runtime::config).
    ///
    /// # Errors
    ///
    /// Always succeeds; the `io::Result` mirrors the sibling constructors
    /// for a uniform `MakeRuntimeFn` signature.
    #[inline]
    pub fn new(config: &Config) -> io::Result<Self> {
        let signaler: &'static Signaler = Box::leak(Box::new(Signaler::new()));
        let signaler_ctx: *mut () = ptr::from_ref(signaler).cast_mut().cast::<()>();
        let executor: &'static Executor = Box::leak(Box::new(Executor::new(signaler_ctx)));
        let spawner = executor.spawner();
        Ok(Self {
            executor: SendWrapper::new(executor),
            signaler,
            spawner: SendWrapper::new(spawner),
            config: config.clone(),
        })
    }

    /// Spawn `task` onto the executor; `TaskStorage` is leaked per spawn so
    /// it outlives the task. Acceptable because test processes are short.
    fn spawn_erased(&self, task: ErasedTask) {
        let storage: &'static TaskStorage<ErasedTask> = Box::leak(Box::new(TaskStorage::new()));
        let token = TaskStorage::spawn(storage, || task)
            .expect("freshly-leaked TaskStorage cannot already be occupied");
        self.spawner.spawn(token);
    }
}

impl<'rt> RuntimeTrait<'rt> for Runtime {
    #[inline]
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future + 'rt,
        F::Output: 'static,
    {
        // The output slot lives on this thread's stack for the whole
        // `drive_until` loop. The spawned task writes into it through a raw
        // pointer carried by `SlotPtr`, which sidesteps the `Send` constraint
        // that `async move` would otherwise demand on `F::Output`.
        let mut slot: Option<F::Output> = None;
        let slot_ptr: SlotPtr<F::Output> = SlotPtr(ptr::from_mut(&mut slot));

        // Lifetime extension: the `drive_until` loop below blocks this thread
        // until the task has completed, so the task can never outlive borrows
        // captured by `fut`.
        let fut_static: Pin<Box<dyn Future<Output = F::Output> + 'static>> = unsafe {
            mem::transmute::<
                Pin<Box<dyn Future<Output = F::Output> + 'rt>>,
                Pin<Box<dyn Future<Output = F::Output> + 'static>>,
            >(Box::pin(fut))
        };

        self.spawn_erased(Box::pin(async move {
            let output = fut_static.await;
            // SAFETY: `slot` is alive for the whole `drive_until` call below
            // (same stack frame) and no other code holds the pointer while
            // this write executes (single-threaded executor).
            unsafe {
                *slot_ptr.0 = Some(output);
            }
        }));

        self.drive_until(|| slot.take())
    }

    #[inline]
    fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    fn name(&self) -> &'static str {
        "embassy::Runtime"
    }

    #[inline]
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'rt {
        // No native timer; delegate to an OS thread. Its `tx.send` wakes the
        // pending task via the receiver's future, which in turn fires
        // `__pender` and unparks the executor loop.
        let (tx, rx) = mpsc::channel::<()>();
        let _unused = thread::spawn(move || {
            thread::sleep(duration);
            let _unused = tx.send(());
        });
        async move {
            let _unused = poll_channel(rx).await;
        }
    }

    #[inline]
    fn spawn<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'rt
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        spawn_task(&self.spawner, fut)
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
        let (tx, rx) = mpsc::channel::<T>();
        let _unused = thread::spawn(move || {
            let _unused = tx.send(func());
        });
        poll_channel(rx)
    }

    #[inline]
    fn spawn_local<F>(&self, fut: F) -> impl Future<Output = Result<F::Output, JoinError>> + 'rt
    where
        F: Future + 'static,
    {
        spawn_task_local(&self.spawner, fut)
    }
}

/// Raw pointer to a caller-owned output slot. Wrapped so the runtime can hand
/// it to an `async move` block without dragging `Send` into the spawn
/// machinery. Dereferenced only at a single write site; see module-level
/// `unsafe_code` allow.
#[derive(Debug)]
struct SlotPtr<T>(*mut Option<T>);

// SAFETY: `SlotPtr` is only handed off between the group thread and its own
// embassy task (same OS thread); no concurrent access ever occurs.
unsafe impl<T> Send for SlotPtr<T> {}

/// Spawn a `Send` future onto the executor and return a `Send` future that
/// yields its output.
fn spawn_task<F>(
    spawner: &Spawner,
    fut: F,
) -> impl Future<Output = Result<F::Output, JoinError>> + Send + 'static
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    let (tx, rx) = mpsc::channel::<F::Output>();
    let erased: ErasedTask = Box::pin(async move {
        let _unused = tx.send(fut.await);
    });
    let storage: &'static TaskStorage<ErasedTask> = Box::leak(Box::new(TaskStorage::new()));
    let token = TaskStorage::spawn(storage, || erased)
        .expect("freshly-leaked TaskStorage cannot already be occupied");
    spawner.spawn(token);
    poll_channel(rx)
}

/// Spawn a `!Send` future onto the executor. Wraps the future and its output
/// in `SendWrapper` so the `TaskStorage`-backed spawn path and the `mpsc`
/// channel both accept them; access stays on the executor thread end to end.
fn spawn_task_local<F>(
    spawner: &Spawner,
    fut: F,
) -> impl Future<Output = Result<F::Output, JoinError>> + 'static
where
    F: Future + 'static,
{
    let wrapped_fut = SendWrapper::new(async move { SendWrapper::new(fut.await) });
    let (tx, rx) = mpsc::channel::<SendWrapper<F::Output>>();
    let erased: ErasedTask = Box::pin(async move {
        let _unused = tx.send(wrapped_fut.await);
    });
    let storage: &'static TaskStorage<ErasedTask> = Box::leak(Box::new(TaskStorage::new()));
    let token = TaskStorage::spawn(storage, || erased)
        .expect("freshly-leaked TaskStorage cannot already be occupied");
    spawner.spawn(token);
    async move {
        match rx.recv() {
            Ok(wrapped) => Ok(wrapped.take()),
            Err(_) => Err(JoinError::cancelled(io::Error::other(
                "embassy local task dropped",
            ))),
        }
    }
}

/// Poll an `mpsc::Receiver` from an async context, yielding between empty
/// attempts so the executor can make progress on the sender side.
async fn poll_channel<T: Send + 'static>(
    rx: mpsc::Receiver<T>,
) -> Result<T, JoinError> {
    loop {
        match rx.try_recv() {
            Ok(val) => return Ok(val),
            Err(mpsc::TryRecvError::Empty) => {
                yield_once().await;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(JoinError::cancelled(io::Error::other(
                    "embassy task dropped before sending result",
                )));
            }
        }
    }
}

/// Single-tick yield implemented via `poll_fn`, used by free helpers that
/// don't have access to `&self` and therefore can't call the trait's default
/// `yield_now`.
#[inline]
fn yield_once() -> impl Future<Output = ()> + Send + 'static {
    let mut yielded = false;
    future::poll_fn(move |cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
}
