//! Process-wide lifecycle event channel.
//!
//! Populated by [`crate::output::init::init`] at startup and read by
//! [`send_lifecycle`]. Kept in a [`OnceLock`] so the
//! [`crate::output::first_poll::FirstPoll`] wrapper doesn't need the
//! sender threaded through every dispatch signature.

use std::sync::OnceLock;

use crossbeam_channel::Sender;

use crate::output::events::LifecycleEvent;

/// Process-wide lifecycle event sender. Set once at capture init,
/// read on every test lifecycle transition.
pub(crate) static LIFECYCLE_SENDER: OnceLock<Sender<LifecycleEvent>> = OnceLock::new();

/// Send a lifecycle event on the global channel. No-op when capture
/// isn't initialised (e.g. on Windows, or when init failed). Never
/// blocks — the channel is unbounded.
///
/// Re-exported as [`crate::output::send_lifecycle`].
#[inline]
pub fn send(event: LifecycleEvent) {
    if let Some(tx) = LIFECYCLE_SENDER.get() {
        let _unused = tx.send(event);
    }
}
