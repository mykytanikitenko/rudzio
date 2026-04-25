//! `rudzio-migrate` — best-effort converter of cargo-style Rust tests
//! into rudzio tests. This module exposes the internals as a library
//! so the thin `src/main.rs` binary and the integration test harness
//! can both reach them.

// All `pub` items in this crate are internal to the
// `rudzio-migrate` workspace member. `unreachable_pub` would force
// `pub(crate)` on every reusable API; scoped here for readability.
// `dead_code` is allowed because some helpers are built ahead of
// their caller as modules grow.
#![allow(
    unreachable_pub,
    dead_code,
    reason = "binary crate: all pub items are internal"
)]

pub mod backup;
pub mod cli;
pub mod detect;
pub mod discovery;
pub mod emit;
pub mod manifest;
pub mod phrase;
pub mod preflight;
pub mod report;
pub mod rewrite;
pub mod runner_scaffold;
pub mod test_context;

pub mod run;
