//! Captures a build-time stamp into `CARGO_RUDZIO_BUILD_STAMP` so the
//! generated aggregator's subdir name encodes which `cargo-rudzio`
//! binary built it.
//!
//! # Why
//!
//! Bug #18: `<aggregator>/target/` is preserved across `cargo rudzio
//! test` invocations. Cargo's fingerprinter keys on source content
//! hashes — NOT on the producing binary's version. When `cargo-rudzio`
//! is rebuilt and emits byte-identical aggregator source for an
//! unchanged consumer workspace, cargo skips the relink and the OLD
//! aggregator binary persists with OLD runner code. Behavioural fixes
//! shipped in `cargo-rudzio` silently fail to take effect.
//!
//! The fix routes around cargo's fingerprinter: the aggregator subdir
//! name includes a hash of this stamp (see `aggregator_subdir_name` in
//! `src/generate.rs`). Different stamps → different subdir → fresh
//! `target/` → fresh compile → fresh binary.
//!
//! # Stamp shape
//!
//! `<CARGO_PKG_VERSION>-<unix_epoch_seconds>`. Version pins the
//! published-release axis; epoch distinguishes dev-iteration builds
//! within the same version.
//!
//! # No `cargo:rerun-if-changed`
//!
//! Cargo's default behaviour reruns `build.rs` whenever any source
//! file in the package changes. That is exactly what we want — every
//! meaningful `cargo-rudzio` change produces a fresh stamp. Adding
//! `cargo::rerun-if-changed=build.rs` would OVERRIDE the default and
//! lock the stamp to `build.rs` changes only, defeating the fix on
//! dev iteration against unchanged `build.rs`.

use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> Result<(), Box<dyn Error>> {
    let version = env!("CARGO_PKG_VERSION");
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system clock before unix epoch: {err}"))?
        .as_secs();
    let stamp = format!("{version}-{epoch}");
    println!("cargo::rustc-env=CARGO_RUDZIO_BUILD_STAMP={stamp}");
    Ok(())
}
