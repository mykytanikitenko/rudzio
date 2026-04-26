//! Future wrapper that announces a test has actually started running.
//!
//! Wraps the test body's future (which the macro-generated dispatch has
//! already `Box::pin`ned, so it is `Unpin`). On the very first call to
//! [`Future::poll`], [`FirstPoll`] emits a
//! [`LifecycleEvent::TestStarted`] on the lifecycle channel, sets the
//! thread-local "current test id" so the panic hook can check it, then
//! delegates to the inner future. Subsequent polls delegate unchanged.
//!
//! The `F: Unpin` bound means this struct is itself `Unpin`
//! (auto-derived when every field is `Unpin`), so the implementation
//! needs no pin-projection and the workspace's
//! `unsafe_code = "deny"` lint stays satisfied.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;
use std::time::Instant;

use super::events::{LifecycleEvent, TestId};

/// Wrap `inner` so the first `poll` emits a `TestStarted` lifecycle
/// event. `test_id` should come from [`TestId::next`] at dispatch
/// site; `module_path`, `test_name`, `runtime_name` are static strings
/// that end up in the event (and, transitively, in the drawer's
/// [`super::events::TestState`]).
#[derive(Debug)]
pub struct FirstPoll<F> {
    fired: bool,
    inner: F,
    module_path: &'static str,
    runtime_name: &'static str,
    test_id: TestId,
    test_name: &'static str,
}

impl<F> FirstPoll<F> {
    #[must_use]
    #[inline]
    pub const fn new(
        inner: F,
        test_id: TestId,
        module_path: &'static str,
        test_name: &'static str,
        runtime_name: &'static str,
    ) -> Self {
        Self {
            inner,
            fired: false,
            test_id,
            module_path,
            test_name,
            runtime_name,
        }
    }
}

impl<F: Future + Unpin> Future for FirstPoll<F> {
    type Output = F::Output;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = Pin::get_mut(self);
        if !this.fired {
            this.fired = true;
            super::panic_hook::set_current_test(Some(this.test_id));
            super::send_lifecycle(LifecycleEvent::TestStarted {
                test_id: this.test_id,
                module_path: this.module_path,
                test_name: this.test_name,
                runtime_name: this.runtime_name,
                thread: thread::current().id(),
                at: Instant::now(),
            });
        }
        Pin::new(&mut this.inner).poll(cx)
    }
}
