//! Capture/render initialisation entry points.

#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use crossbeam_channel::{bounded, unbounded};

use crate::config::{Config, OutputMode};
use crate::output::capture_guard::CaptureGuard;
#[cfg(unix)]
use crate::output::color;
#[cfg(unix)]
use crate::output::events::{LifecycleEvent, PipeChunk, StdStream};
#[cfg(unix)]
use crate::output::lifecycle::LIFECYCLE_SENDER;
use crate::output::panic_hook;
#[cfg(unix)]
use crate::output::pipe;
#[cfg(unix)]
use crate::output::reader;
#[cfg(unix)]
use crate::output::render;

/// Initialise capture + render subsystems.
///
/// Returns a no-op guard when [`crate::config::OutputMode::Plain`] is
/// selected (the runner's reporter prints cargo-test-style lines
/// itself — the drawer is unnecessary overhead) or when the target
/// isn't Unix.
///
/// # Errors
///
/// Returns an error when the FD-swapping `pipe()` setup fails, when
/// spawning a reader/drawer thread fails, or when any underlying
/// `dup2`/`pipe2` syscall returns an OS error.
#[inline]
pub fn init(config: &Config) -> io::Result<CaptureGuard> {
    if matches!(config.output_mode, OutputMode::Plain) {
        // Install the panic hook even in plain mode — without it, a
        // panic on a background thread spawned by user setup wouldn't
        // bump the unattributed-panic counter and the runner's
        // end-of-run safety net would silently miss it. Plain mode
        // doesn't capture stdio, so pass `None` for the FD restore.
        panic_hook::install(None);
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

/// Unix-only init path: swap FDs 1/2 onto pipes and spawn the
/// drawer/readers wired to the captured streams.
#[cfg(unix)]
fn init_unix(config: &Config) -> io::Result<CaptureGuard> {
    use std::io::IsTerminal as _;

    // Snapshot TTY status of the original stdout BEFORE capture swaps
    // FDs — afterwards `is_terminal()` on FD 1 reports the pipe, not
    // the terminal.
    let stdout_is_tty = io::stdout().is_terminal();
    let color = color::Policy::resolve(config.color, stdout_is_tty, &config.env);

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
    let terminal = File::from(capture.drawer_terminal);

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
    panic_hook::install(Some(Arc::clone(&saved)));

    Ok(CaptureGuard::new(
        saved,
        reader_stdout,
        reader_stderr,
        drawer_handle,
        shutdown_tx,
    ))
}
