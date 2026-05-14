//! Libtest-compatible `--logfile <PATH>` sink.
//!
//! When the runner is invoked with `--logfile <PATH>`, every test
//! finish emits one line to `<PATH>` in the libtest log format:
//!
//! ```text
//! ok    <qualified_name>
//! failed <qualified_name>
//! ignored <qualified_name>
//! ```
//!
//! Open semantics match libtest: the file is truncated on open so a
//! re-run does not append to a prior run's log. Open failures are
//! surfaced via [`crate::output::write_stderr`] and the writer
//! degrades to a no-op rather than aborting the run, since the user's
//! primary expectation (run the tests, see results on stdout) is
//! independent of whether the side-channel log was writable.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::output::writers::write_stderr;

/// Append-only sink for libtest-format test result lines.
#[derive(Debug)]
pub struct Writer {
    /// `Some` when an output file was opened successfully; `None` when
    /// the user did not pass `--logfile` or the open failed.
    inner: Option<Mutex<BufWriter<File>>>,
}

impl Writer {
    /// Flush buffered output to the underlying file. Called at run end
    /// from the runner so a buffered tail is not lost when the process
    /// exits.
    #[inline]
    pub fn flush(&self) {
        let Some(mutex) = self.inner.as_ref() else {
            return;
        };
        let _ignored = mutex.lock().unwrap_or_else(PoisonError::into_inner).flush();
    }

    /// `true` when an output file was opened and lines will be written.
    #[inline]
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Open `requested` for write+truncate. When `requested` is `None`, returns
    /// a disabled writer whose [`Self::write_line`] is a no-op.
    ///
    /// On open failure (permission denied, parent missing, …) writes a
    /// single warning line to stderr and returns a disabled writer so
    /// the run can proceed. The user's primary signal — pass/fail on
    /// stdout — is independent of the logfile side channel.
    #[inline]
    #[must_use]
    pub fn open(requested: Option<&Path>) -> Self {
        let Some(path) = requested else {
            return Self { inner: None };
        };
        match OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
        {
            Ok(file) => Self {
                inner: Some(Mutex::new(BufWriter::new(file))),
            },
            Err(err) => {
                write_stderr(&format!(
                    "rudzio: --logfile {} could not be opened: {err}\n",
                    path.display(),
                ));
                Self { inner: None }
            }
        }
    }

    /// Append `<status> <qualified_name>\n` to the logfile. No-op when
    /// the writer is disabled.
    ///
    /// I/O errors are silently dropped so a transient write failure
    /// (disk full mid-run, etc.) cannot promote into a test-run
    /// failure. The side-channel guarantees are best-effort by design.
    #[inline]
    pub fn write_line(&self, status: &str, qualified_name: &str) {
        let Some(mutex) = self.inner.as_ref() else {
            return;
        };
        let _ignored = writeln!(
            *mutex.lock().unwrap_or_else(PoisonError::into_inner),
            "{status} {qualified_name}",
        );
    }
}
