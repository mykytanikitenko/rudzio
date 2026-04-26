//! Dedicated pipe reader threads.
//!
//! One thread per captured pipe. Each runs a blocking `read()` loop,
//! packaging each chunk as a [`PipeChunk`] and forwarding it to the
//! drawer via a [`crossbeam_channel::Sender`]. On shutdown, the
//! [`crate::output::CaptureGuard`] drops the write-end references
//! (FDs 1 and 2 are restored to the saved originals by
//! [`crate::output::pipe::restore`]); with no more writers the kernel
//! returns 0 from `read`, which is the thread's exit signal.
//!
//! Reads are intentionally into a fresh heap buffer per iteration so
//! the `PipeChunk` can be sent by move without copying. The reader
//! never does any other work — it exists purely to drain the pipe as
//! fast as the kernel produces data, so the pipe buffer rarely fills
//! and test `write`s rarely block.

use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::thread::{self, JoinHandle};

use crossbeam_channel::Sender;

use super::events::{PipeChunk, StdStream};

const READ_BUF_SIZE: usize = 16 * 1024;

/// Spawn a reader thread for one pipe. The thread takes ownership of
/// `fd` and exits when `read` returns 0 (writer end closed) or the
/// drawer hangs up the receiver.
#[inline]
pub fn spawn(fd: OwnedFd, stream: StdStream, tx: Sender<PipeChunk>) -> io::Result<JoinHandle<()>> {
    let name = match stream {
        StdStream::Stdout => "rudzio-output-reader-stdout",
        StdStream::Stderr => "rudzio-output-reader-stderr",
    };
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut file = File::from(fd);
        let mut buf = vec![0_u8; READ_BUF_SIZE];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = PipeChunk {
                        stream,
                        bytes: buf[..n].to_vec(),
                    };
                    if tx.send(chunk).is_err() {
                        // Drawer hung up; no point reading any more.
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}
