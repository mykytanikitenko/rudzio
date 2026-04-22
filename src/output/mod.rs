//! Live-region test output with per-test stdio capture.
//!
//! The runner calls [`init`] once at startup with the resolved
//! [`Config`]. On Unix terminals this returns a [`CaptureGuard`]
//! that owns:
//!
//! - Saved originals of FDs 1 and 2 (via [`pipe::SavedFds`]).
//! - The pipe reader threads that drain the captured FDs.
//! - The drawer thread that consumes lifecycle events + captured
//!   bytes and renders a live region + history region (or linear
//!   plain output) to the real terminal.
//! - A shared reference to a lifecycle-event [`Sender`] that the
//!   [`first_poll::FirstPoll`] wrapper, the macro-generated bench
//!   progress callback, and the runtime threads publish to via
//!   [`send_lifecycle`].
//! - A custom [`panic_hook`] that restores FDs if the panic came
//!   from outside any captured test.
//!
//! Dropping the guard is the single point of cleanup: it signals
//! shutdown, joins the drawer (which prints the final summary),
//! closes the pipe read ends (which causes the reader threads to
//! exit cleanly when `read` returns 0), and restores FDs 1 and 2
//! via [`pipe::SavedFds::restore`]. The restore is idempotent with
//! the panic-hook path.
//!
//! On non-Unix targets (or when `init` fails), the guard is a
//! no-op stub; everything still compiles and tests still run, just
//! without the fancy rendering — `println!`s go to stdout directly.

pub mod color;
pub mod events;
pub mod first_poll;
pub mod panic_hook;
#[cfg(unix)]
pub mod pipe;
#[cfg(unix)]
pub mod reader;
pub mod render;

use std::io;
use std::sync::OnceLock;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::thread::JoinHandle;

use crossbeam_channel::Sender;
#[cfg(unix)]
use crossbeam_channel::{bounded, unbounded};

use crate::config::Config;

pub use events::{LifecycleEvent, PipeChunk, StdStream, TestId, TestState, TestStateKind};

/// Process-wide lifecycle event sender, populated by [`init`] and
/// read by [`send_lifecycle`]. Kept in a `OnceLock` so the
/// [`first_poll::FirstPoll`] wrapper doesn't need the sender threaded
/// through every dispatch signature.
static LIFECYCLE_SENDER: OnceLock<Sender<LifecycleEvent>> = OnceLock::new();

/// Send a lifecycle event on the global channel. No-op when capture
/// isn't initialised (e.g. on Windows, or when init failed). Never
/// blocks — the channel is unbounded.
pub fn send_lifecycle(event: LifecycleEvent) {
    if let Some(tx) = LIFECYCLE_SENDER.get() {
        let _unused = tx.send(event);
    }
}

/// Owner of capture/render infrastructure. Dropping it restores FDs
/// 1 and 2, joins the drawer, and joins the pipe readers.
#[derive(Debug)]
#[cfg(unix)]
pub struct CaptureGuard {
    saved: Arc<pipe::SavedFds>,
    reader_stdout: Option<JoinHandle<()>>,
    reader_stderr: Option<JoinHandle<()>>,
    drawer: Option<JoinHandle<()>>,
    shutdown_tx: Option<Sender<()>>,
}

/// Non-Unix guard — everything is `None`/no-op. Keeps the public
/// type surface identical across platforms so downstream code doesn't
/// have to `cfg!` around it.
#[derive(Debug, Default)]
#[cfg(not(unix))]
pub struct CaptureGuard {
    _private: (),
}

#[cfg(unix)]
impl CaptureGuard {
    /// Build a no-op guard used for `OutputMode::Plain` or when Unix
    /// FD swapping isn't available. `SavedFds::new(-1, -1)` means the
    /// restore path (idempotent) no-ops.
    fn plain() -> Self {
        Self {
            saved: Arc::new(pipe::SavedFds::new(-1, -1)),
            reader_stdout: None,
            reader_stderr: None,
            drawer: None,
            shutdown_tx: None,
        }
    }
}

#[cfg(not(unix))]
impl CaptureGuard {
    fn plain() -> Self {
        Self::default()
    }
}

#[cfg(unix)]
impl Drop for CaptureGuard {
    fn drop(&mut self) {
        // Step 1 — stop the drawer. Closing the shutdown channel
        // makes its `select!` arm return Err; it then drains pending
        // events and prints the summary before exiting.
        if let Some(tx) = self.shutdown_tx.take() {
            drop(tx);
        }
        if let Some(h) = self.drawer.take() {
            let _unused = h.join();
        }
        // Step 2 — restore FDs. Idempotent with the panic hook's
        // restore path; whichever runs first wins.
        self.saved.restore();
        // Step 3 — join reader threads. They exit on `read` returning
        // 0, which happens when the pipe write ends close. The pipe
        // write ends in FDs 1/2 were replaced by dup2(saved, 1|2) in
        // the restore call above, so the original write end of each
        // pipe has no more referents and the readers see EOF.
        if let Some(h) = self.reader_stdout.take() {
            let _unused = h.join();
        }
        if let Some(h) = self.reader_stderr.take() {
            let _unused = h.join();
        }
    }
}

/// Initialise capture + render subsystems.
///
/// Returns a no-op guard when [`crate::config::OutputMode::Plain`] is
/// selected (the runner's reporter prints cargo-test-style lines
/// itself — the drawer is unnecessary overhead) or when the target
/// isn't Unix.
pub fn init(config: &Config) -> io::Result<CaptureGuard> {
    if matches!(config.output_mode, crate::config::OutputMode::Plain) {
        return Ok(CaptureGuard::plain());
    }
    #[cfg(unix)]
    {
        init_unix(config)
    }
    #[cfg(not(unix))]
    {
        let _unused = config;
        Ok(CaptureGuard::plain())
    }
}

#[cfg(unix)]
fn init_unix(config: &Config) -> io::Result<CaptureGuard> {
    use std::io::IsTerminal as _;

    // Snapshot TTY status of the original stdout BEFORE capture swaps
    // FDs — afterwards `is_terminal()` on FD 1 reports the pipe, not
    // the terminal.
    let stdout_is_tty = io::stdout().is_terminal();
    let color = color::ColorPolicy::resolve(config.color, stdout_is_tty, &config.env);

    let capture = pipe::init()?;
    let (lifecycle_tx, lifecycle_rx) = unbounded::<LifecycleEvent>();
    let (pipe_tx, pipe_rx) = unbounded::<PipeChunk>();
    let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

    // Publish lifecycle sender BEFORE spawning reader/drawer threads
    // so TestStarted events from FirstPoll are never dropped.
    let _unused = LIFECYCLE_SENDER.set(lifecycle_tx);

    let reader_stdout = reader::spawn(capture.stdout_read, StdStream::Stdout, pipe_tx.clone())?;
    let reader_stderr = reader::spawn(capture.stderr_read, StdStream::Stderr, pipe_tx)?;

    // Wrap the drawer terminal OwnedFd in a std::fs::File. File's Drop
    // closes the FD when the drawer thread exits — independent of the
    // SavedFds restore path.
    let terminal = std::fs::File::from(capture.drawer_terminal);

    let drawer = render::Drawer::new(
        lifecycle_rx,
        pipe_rx,
        shutdown_rx,
        terminal,
        config.output_mode,
        config.format,
        color,
    );
    let drawer_handle = render::spawn_drawer(drawer)?;

    let saved = Arc::new(capture.saved);
    panic_hook::install(Arc::clone(&saved));

    Ok(CaptureGuard {
        saved,
        reader_stdout: Some(reader_stdout),
        reader_stderr: Some(reader_stderr),
        drawer: Some(drawer_handle),
        shutdown_tx: Some(shutdown_tx),
    })
}
