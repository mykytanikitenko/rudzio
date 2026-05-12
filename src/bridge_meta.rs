//! Bridge introspection consts used by rudzio's own integration tests.
//!
//! Verifies that `cargo:rustc-env=CARGO_MANIFEST_DIR=<override>`
//! directives in a generated bridge `build.rs` reach both the
//! `env!` channel rustc inlines at compile time and the
//! `std::env::var` channel proc-macros consult at expansion time.
//!
//! Hidden from rustdoc — not a stability guarantee.

/// Captures `CARGO_MANIFEST_DIR` as rustc saw it when compiling
/// rudzio's `src/lib.rs`, via the `env!` macro (the rustc-tracked
/// compile-time env channel).
#[doc(hidden)]
pub const __BRIDGE_OBSERVED_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Captures `CARGO_MANIFEST_DIR` as proc-macros saw it via
/// `std::env::var` at expansion time of rudzio's `src/lib.rs`.
///
/// `cargo:rustc-env=CARGO_MANIFEST_DIR=<override>` from a bridge
/// `build.rs` reaches `env!` (the channel above), but cargo may pass
/// reserved env vars to rustc through a separate mechanism that
/// proc-macros' `std::env::var` doesn't see. Third-party
/// proc-macros like `refinery::embed_migrations!` and
/// `sqlx::migrate!` resolve their path arguments via `std::env::var`,
/// not `env!`, so this const exists to verify both channels carry
/// the same override.
#[doc(hidden)]
pub const __BRIDGE_PROC_MACRO_OBSERVED_MANIFEST_DIR: &str =
    rudzio_macro::__proc_macro_env!("CARGO_MANIFEST_DIR");
