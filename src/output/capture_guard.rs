//! Owner of capture/render infrastructure.
//!
//! Dropping a [`CaptureGuard`] is the single point of cleanup for the
//! Unix capture path: it signals shutdown, joins the drawer (which
//! prints the final summary), closes the pipe read ends so the
//! reader threads exit cleanly when `read` returns 0, and restores
//! FDs 1 and 2 via [`crate::output::pipe::SavedFds::restore`]. The
//! restore is idempotent with the panic-hook path.
//!
//! On non-Unix targets (or when capture init fails), the guard is a
//! no-op stub; everything still compiles and tests still run, just
//! without the fancy rendering — `println!`s go to stdout directly.

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::thread::JoinHandle;

#[cfg(unix)]
use crossbeam_channel::Sender;

#[cfg(unix)]
use crate::output::pipe;

/// Owner of capture/render infrastructure. Dropping it restores FDs
/// 1 and 2, joins the drawer, and joins the pipe readers.
#[derive(Debug)]
#[cfg(unix)]
pub struct CaptureGuard {
    /// Join handle for the drawer thread; taken in `Drop`.
    drawer: Option<JoinHandle<()>>,
    /// Join handle for the stderr pipe-reader thread.
    reader_stderr: Option<JoinHandle<()>>,
    /// Join handle for the stdout pipe-reader thread.
    reader_stdout: Option<JoinHandle<()>>,
    /// Saved FDs restored on drop or panic; shared with the panic
    /// hook so whichever path runs first wins.
    saved: Arc<pipe::SavedFds>,
    /// Shutdown signal that ends the drawer's `select!` loop.
    shutdown_tx: Option<Sender<()>>,
}

/// Non-Unix guard — everything is `None`/no-op. Keeps the public
/// type surface identical across platforms so downstream code doesn't
/// have to `cfg!` around it.
#[derive(Debug)]
#[cfg(not(unix))]
pub struct CaptureGuard {
    _private: (),
}

#[cfg(unix)]
impl CaptureGuard {
    /// Build a fully-wired guard owning every spawned thread plus the
    /// saved-FD handle. Bundled here so [`crate::output::init`] never
    /// names the private fields directly.
    #[inline]
    pub(crate) const fn new(
        saved: Arc<pipe::SavedFds>,
        reader_stdout: JoinHandle<()>,
        reader_stderr: JoinHandle<()>,
        drawer: JoinHandle<()>,
        shutdown_tx: Sender<()>,
    ) -> Self {
        Self {
            saved,
            reader_stdout: Some(reader_stdout),
            reader_stderr: Some(reader_stderr),
            drawer: Some(drawer),
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Build a no-op guard used for `OutputMode::Plain` or when Unix
    /// FD swapping isn't available. `SavedFds::new(-1, -1)` means the
    /// restore path (idempotent) no-ops.
    #[inline]
    pub(crate) fn plain() -> anyhow::Result<Self> {
        Ok(Self {
            saved: Arc::new(pipe::SavedFds::new(-1, -1)),
            reader_stdout: None,
            reader_stderr: None,
            drawer: None,
            shutdown_tx: None,
        })
    }
}

#[cfg(not(unix))]
impl CaptureGuard {
    #[inline]
    pub(crate) const fn plain() -> Self {
        Self { _private: () }
    }
}

#[cfg(unix)]
impl Drop for CaptureGuard {
    #[inline]
    fn drop(&mut self) {
        // Step 1 — stop the drawer. Closing the shutdown channel
        // makes its `select!` arm return Err; it then drains pending
        // events and prints the summary before exiting.
        if let Some(tx) = self.shutdown_tx.take() {
            drop(tx);
        }
        if let Some(handle) = self.drawer.take() {
            let _unused = handle.join();
        }
        // Step 2 — restore FDs. Idempotent with the panic hook's
        // restore path; whichever runs first wins.
        self.saved.restore();
        // Step 3 — abandon the reader threads, do NOT `.join()` them.
        //
        // The original intent was: after step 2 closes our copy of
        // each pipe write end (via `dup2(saved, 1|2)`), the reader's
        // `read()` sees EOF and the thread exits cleanly, so the
        // join returns. That holds when nothing else copied FD 1 / 2.
        //
        // **Bug #13**: `dup2` clears `FD_CLOEXEC` on its destination,
        // and `pipe(2)` likewise produces FDs without `O_CLOEXEC`,
        // so any child process the user's `setup()` spawned with
        // `Stdio::inherit()` (the default) inherited a live copy of
        // the pipe write end. The kernel won't report EOF on the
        // reader's `read()` until *every* write-end FD is closed —
        // the child's copy survives until the child itself exits.
        // Multi-binary monolith runs deadlocked here: the second
        // group's suite was still alive when the first group's
        // guard dropped, so its child held the pipes, so `join()`
        // never returned, so the whole process hung.
        //
        // The fix is to detach the reader threads. By this point:
        //   - The drawer's receiver was dropped in step 1, so any
        //     subsequent `tx.send(chunk)` from the readers returns
        //     `Err` and the reader breaks the loop on its own.
        //   - FDs 1 and 2 already point at the original stdout/
        //     stderr (step 2), so the runner's own writes after
        //     this drop go straight to the terminal.
        //   - The reader threads hold no shared mutable state the
        //     rest of the process needs cleaned up. When the
        //     process exits, the OS reaps them.
        //
        // For long-lived processes that call `runner::run` more
        // than once, this leaks one pair of reader threads per run
        // until process exit — an acceptable tradeoff vs hanging
        // forever, and the runner is overwhelmingly invoked at
        // most once per process anyway.
        let _detached_stdout = self.reader_stdout.take();
        let _detached_stderr = self.reader_stderr.take();
    }
}
