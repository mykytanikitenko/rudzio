//! Unix-only FD management for process-wide stdio capture.
//!
//! At runner startup, [`init`] replaces FDs 1 and 2 with the write ends
//! of anonymous pipes and hands back the read ends plus the saved
//! originals wrapped in a [`Capture`] bundle. The read ends are handed
//! to [`crate::output::reader`]; the saved originals are held by the
//! [`crate::output::CaptureGuard`] and restored on drop via
//! [`restore`].
//!
//! The whole module is Unix-only — the live-output feature falls back
//! to plain mode with no capture on Windows (see
//! [`crate::output::init`]). `libc` FFI requires a module-level
//! `#![allow(unsafe_code)]`; the pattern mirrors the runtime-local
//! allow in `src/runtime/embassy.rs`.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// Target kernel pipe buffer size. Linux honours this up to
/// `/proc/sys/fs/pipe-max-size` (typically 1 MiB without privileges);
/// on other Unix platforms [`set_pipe_size`] is a no-op and the
/// platform default applies.
const PIPE_SIZE: libc::c_int = 1 << 20;

/// The concrete capture state returned by [`init`]. Owned by
/// [`crate::output::CaptureGuard`], which restores the originals on
/// drop via [`SavedFds::restore`].
#[derive(Debug)]
pub struct Capture {
    /// A third dup of the original stdout (taken before the swap),
    /// handed to the drawer so it can write the live region and
    /// history to the real terminal. Owned separately from
    /// [`SavedFds`] so the drawer's File can close independently
    /// when the drawer exits without racing the restore path.
    pub drawer_terminal: OwnedFd,
    /// Saved originals, ready for [`SavedFds::restore`]. Wrapped so
    /// the panic hook can share the same state via `Arc<SavedFds>`
    /// and whichever path restores first takes ownership.
    pub saved: SavedFds,
    /// Read end of the pipe whose write end is now FD 2.
    pub stderr_read: OwnedFd,
    /// Read end of the pipe whose write end is now FD 1.
    pub stdout_read: OwnedFd,
}

/// Shared storage for the pre-capture FDs.
///
/// Holding one of these inside an `Arc` lets the
/// [`crate::output::CaptureGuard`] and the custom panic hook both
/// try to restore; the internal atomic swap makes subsequent calls
/// no-op.
#[derive(Debug)]
pub struct SavedFds {
    /// Saved-original FD 2 (stderr); set to `-1` once restored.
    stderr: AtomicI32,
    /// Saved-original FD 1 (stdout); set to `-1` once restored.
    stdout: AtomicI32,
}

impl SavedFds {
    /// Store the saved FDs. Both must be valid, dup'd copies of
    /// FDs 1 and 2 from before the capture swap.
    #[must_use]
    #[inline]
    pub const fn new(stdout: RawFd, stderr: RawFd) -> Self {
        Self {
            stdout: AtomicI32::new(stdout),
            stderr: AtomicI32::new(stderr),
        }
    }

    /// Restore FDs 1 and 2 from the saved originals and close the
    /// saved copies. Idempotent — concurrent calls (the normal drop
    /// path racing with the panic hook) each atomically swap the
    /// stored FD to `-1`; only the winner runs the real `dup2` +
    /// `close` pair. The loser sees `-1` and returns immediately.
    #[inline]
    pub fn restore(&self) {
        let stdout = self.stdout.swap(-1, Ordering::AcqRel);
        let stderr = self.stderr.swap(-1, Ordering::AcqRel);
        if stdout != -1 {
            // SAFETY: stdout was obtained from libc::dup at construction time
            // and won the AcqRel swap above, so we hold the only live
            // reference to it. dup2 with STDOUT_FILENO is a defined syscall.
            let _stdout_dup_ret: libc::c_int =
                unsafe { libc::dup2(stdout, libc::STDOUT_FILENO) };
            // SAFETY: same exclusive ownership as the dup2 above; close on a
            // valid FD is defined.
            let _stdout_close_ret: libc::c_int = unsafe { libc::close(stdout) };
        }
        if stderr != -1 {
            // SAFETY: stderr was obtained from libc::dup at construction time
            // and won the AcqRel swap above, so we hold the only live
            // reference to it. dup2 with STDERR_FILENO is a defined syscall.
            let _stderr_dup_ret: libc::c_int =
                unsafe { libc::dup2(stderr, libc::STDERR_FILENO) };
            // SAFETY: same exclusive ownership as the dup2 above; close on a
            // valid FD is defined.
            let _stderr_close_ret: libc::c_int = unsafe { libc::close(stderr) };
        }
    }
}

/// Save FDs 1 and 2 and install anonymous pipes in their place.
///
/// Hands back the read ends plus the saved originals. Best-effort
/// enlargement of the pipe buffers to [`PIPE_SIZE`] — ignored silently
/// if the platform or system policy refuses.
///
/// # Errors
///
/// Returns an error when any of the underlying `dup` / `pipe` /
/// `dup2` syscalls fails (e.g. FD-table exhaustion).
#[inline]
pub fn init() -> io::Result<Capture> {
    let saved_stdout = dup(libc::STDOUT_FILENO)?;
    let saved_stderr = match dup(libc::STDERR_FILENO) {
        Ok(fd) => fd,
        Err(err) => {
            close(saved_stdout);
            return Err(err);
        }
    };
    // Third dup: the drawer needs an FD to write the live region to
    // the real terminal, independent from SavedFds so neither can
    // close the other's FD.
    let drawer_terminal_fd = match dup(libc::STDOUT_FILENO) {
        Ok(fd) => fd,
        Err(err) => {
            close(saved_stdout);
            close(saved_stderr);
            return Err(err);
        }
    };
    // SAFETY: drawer_terminal_fd was just returned by libc::dup and
    // has no other owner.
    let drawer_terminal = unsafe { OwnedFd::from_raw_fd(drawer_terminal_fd) };

    let (stdout_read, stdout_write) = match pipe() {
        Ok(pair) => pair,
        Err(err) => {
            close(saved_stdout);
            close(saved_stderr);
            return Err(err);
        }
    };
    let (stderr_read, stderr_write) = match pipe() {
        Ok(pair) => pair,
        Err(err) => {
            close(saved_stdout);
            close(saved_stderr);
            drop(stdout_read);
            drop(stdout_write);
            return Err(err);
        }
    };

    // Expand pipe buffer if supported (Linux); harmless no-op elsewhere.
    let _stdout_pipe_resize: io::Result<()> = set_pipe_size(stdout_write.as_raw_fd(), PIPE_SIZE);
    let _stderr_pipe_resize: io::Result<()> = set_pipe_size(stderr_write.as_raw_fd(), PIPE_SIZE);

    // Install the write ends over FDs 1 and 2. `dup2` closes whatever
    // was at the target FD before duplicating — that's what we want.
    if let Err(err) = dup2(stdout_write.as_raw_fd(), libc::STDOUT_FILENO) {
        close(saved_stdout);
        close(saved_stderr);
        return Err(err);
    }
    if let Err(err) = dup2(stderr_write.as_raw_fd(), libc::STDERR_FILENO) {
        // Try to roll back FD 1 before reporting.
        let _unused = dup2(saved_stdout, libc::STDOUT_FILENO);
        close(saved_stdout);
        close(saved_stderr);
        return Err(err);
    }

    // The original write-end FDs are redundant now that FDs 1 and 2
    // reference the same pipe write end. Dropping the OwnedFds closes
    // them.
    drop(stdout_write);
    drop(stderr_write);

    Ok(Capture {
        stdout_read,
        stderr_read,
        drawer_terminal,
        saved: SavedFds::new(saved_stdout, saved_stderr),
    })
}

/// Duplicate `fd` via `libc::dup`, returning an owned raw FD or the
/// last OS error.
fn dup(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: libc::dup of a valid FD (1 or 2) returns a new FD or
    // -1 on error. The returned FD is owned by the caller.
    let new_fd = unsafe { libc::dup(fd) };
    if new_fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(new_fd)
    }
}

/// Duplicate `src` onto `dst` via `libc::dup2`, surfacing OS errors.
fn dup2(src: RawFd, dst: RawFd) -> io::Result<()> {
    // SAFETY: libc::dup2 with valid src+dst FDs is defined; the kernel
    // atomically closes dst (if open) and duplicates src into it.
    let ret = unsafe { libc::dup2(src, dst) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Close `fd` via `libc::close`, ignoring any error since drop paths
/// have nowhere to report it.
fn close(fd: RawFd) {
    // SAFETY: closing a valid FD is defined; errors are ignored
    // because Drop paths have nowhere to report them.
    unsafe {
        let _unused = libc::close(fd);
    }
}

/// Create an anonymous pipe and return its read and write ends as
/// owned FDs.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0_i32; 2];
    // SAFETY: libc::pipe writes two FDs into the provided 2-element
    // array, or returns -1 on error.
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both FDs were just produced by libc::pipe and have no
    // other owners; wrapping them in OwnedFd takes ownership so Drop
    // closes them.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: same as above — fds[1] was just produced by libc::pipe
    // with no other owners.
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read, write))
}

/// Linux-only: enlarge the pipe buffer for `fd` via `F_SETPIPE_SZ`.
#[cfg(target_os = "linux")]
fn set_pipe_size(fd: RawFd, size: libc::c_int) -> io::Result<()> {
    // SAFETY: F_SETPIPE_SZ is a Linux fcntl command accepting an int.
    // Returns -1 on error which we convert to io::Error.
    let ret = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, size) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "Mirrors the Linux signature so the call site is platform-agnostic."
)]
const fn set_pipe_size(_fd: RawFd, _size: libc::c_int) -> io::Result<()> {
    // F_SETPIPE_SZ is Linux-specific. Other Unixes inherit their
    // platform pipe buffer (macOS: 16 KiB by default; bumpable only
    // via kernel tunables). Documented as best-effort.
    Ok(())
}
