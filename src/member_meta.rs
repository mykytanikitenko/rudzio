//! Hosts the `manifest_dir!()` macro.
//!
//! Under v2 each bridge's `build.rs` emits
//! `cargo:rustc-env=CARGO_MANIFEST_DIR=<member dir>` so a bare
//! `env!("CARGO_MANIFEST_DIR")` inside member src or `#[path]`-included
//! test proxies resolves to the member's real manifest dir
//! transparently — no runtime registry, no `module_path!()` rewriting.
//! The macro stays as the public API surface so existing call sites
//! keep working.

/// Return the member's `CARGO_MANIFEST_DIR` as a `PathBuf`.
///
/// Under stock `cargo test -p <member>`, `env!("CARGO_MANIFEST_DIR")`
/// returns the member's manifest dir directly. Under
/// `cargo rudzio test`, the bridge's `build.rs` emits
/// `cargo:rustc-env=CARGO_MANIFEST_DIR=<member dir>` so the same env!()
/// resolves to the same value inside the bridge's compile unit — the
/// macro just wraps that lookup in a `PathBuf` for ergonomics.
///
/// ```rust,ignore
/// let fixtures = rudzio::manifest_dir!().join("fixtures");
/// ```
#[macro_export]
macro_rules! manifest_dir {
    () => {
        ::std::path::PathBuf::from(::std::env!("CARGO_MANIFEST_DIR"))
    };
}
