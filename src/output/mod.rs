//! Live-region test output with per-test stdio capture.
//!
//! The runner calls [`init`] once at startup with the resolved
//! [`crate::config::Config`]. On Unix terminals this returns a
//! [`CaptureGuard`] that owns:
//!
//! - Saved originals of FDs 1 and 2 (via [`pipe::SavedFds`]).
//! - The pipe reader threads that drain the captured FDs.
//! - The drawer thread that consumes lifecycle events + captured
//!   bytes and renders a live region + history region (or linear
//!   plain output) to the real terminal.
//! - A shared reference to a lifecycle-event [`crossbeam_channel::Sender`]
//!   that the [`first_poll::FirstPoll`] wrapper, the macro-generated
//!   bench progress callback, and the runtime threads publish to via
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

pub mod capture_guard;
pub mod color;
pub mod events;
pub mod first_poll;
pub mod init;
pub mod lifecycle;
pub mod panic_hook;
#[cfg(unix)]
pub mod pipe;
#[cfg(unix)]
pub mod reader;
pub mod render;
pub mod writers;

pub use capture_guard::CaptureGuard;
pub use events::{LifecycleEvent, PipeChunk, StdStream, TestId, TestState, TestStateKind};
pub use init::init;
pub use lifecycle::send as send_lifecycle;
pub use writers::{write_stderr, write_stdout};
