//! Regression reproducer for the STALE-BINARY mechanism that made
//! cargo-rudzio e2e tests report a false green.
//!
//! # Mechanism
//!
//! The e2e specs spawn the `cargo-rudzio` binary as a subprocess via
//! `env!("CARGO_BIN_EXE_cargo-rudzio")`. Cargo expands that macro at
//! COMPILE time of this integration-test binary, baking in an ABSOLUTE
//! path of the form `<CARGO_TARGET_DIR>/debug/cargo-rudzio`, where
//! `<CARGO_TARGET_DIR>` is whatever the invoking cargo used (see
//! `cross_bridge_spec.rs` and `cargo-rudzio-test-helpers/src/lib.rs`).
//!
//! The ambient environment pins that directory to a shared, persistent,
//! cross-worktree cache:
//!
//! * `CARGO_TARGET_DIR=/var/mnt/data-cache/cargo-target` (exported), and
//! * `~/.cargo/config.toml` / `$CARGO_HOME/config.toml` set
//!   `build.build-dir = "/var/mnt/data-cache/cargo/build-dir"` and
//!   `build.rustc-wrapper = "sccache"`.
//!
//! Because `CARGO_BIN_EXE_cargo-rudzio` is baked once at compile time, a
//! test binary compiled against the shared dir will spawn the binary that
//! HAPPENS to live at that shared path — not necessarily the binary a
//! subagent just built. When a prior checkout/task left a stale binary in
//! the shared cache, the freshly-built fix is silently bypassed and a red
//! fix reports green (the stale binary still exhibits the old behavior).
//!
//! # Harden (not yet implemented)
//!
//! The e2e invocation must set a task-local `CARGO_TARGET_DIR` AND
//! `CARGO_BUILD_BUILD_DIR` and must not inherit the shared ambient ones;
//! and/or the harness must assert that the binary it spawns is freshly
//! built from the current source (hash-compare or re-link before spawning).
//!
//! This test asserts the invariant directly and is RED under the current
//! un-hardened invocation: the baked path resolves into the shared cache.

use rudzio::common::context::Suite;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;

/// Shared-cache roots a hardened harness must never resolve its binary into.
/// These are the ambient `CARGO_TARGET_DIR` and `build.build-dir` values
/// pinned by the environment (`/var/mnt/data-cache/...`).
const SHARED_CACHE_ROOTS: &[&str] = &["/var/mnt/data-cache/"];

#[rudzio::suite([
    (runtime = FuturesThreadPool::new, suite = Suite, test = Test),
])]
mod tests {
    #[rudzio::test]
    fn baked_cargo_rudzio_binary_is_not_shared_cache() -> anyhow::Result<()> {
        let baked = env!("CARGO_BIN_EXE_cargo-rudzio");
        let shared = super::SHARED_CACHE_ROOTS
            .iter()
            .find(|root| baked.starts_with(**root));
        anyhow::ensure!(
            shared.is_none(),
            "STALE-BINARY: CARGO_BIN_EXE_cargo-rudzio = {baked} resolves into a shared \
             persistent cache ({:?}). A stale/foreign binary built by a prior checkout may be \
             spawned, making a red fix report green. Harden the invocation with a task-local \
             CARGO_TARGET_DIR and CARGO_BUILD_BUILD_DIR (and/or assert freshness before \
             spawning).",
            shared
        );
        Ok(())
    }
}
