//! Proc-macro entry points.
//!
//! Rust requires every `#[proc_macro_*]` function to live at the crate
//! root of a `proc-macro = true` crate. Bodies in this file therefore
//! exist only to forward into [`rudzio_macro_internals`] across the
//! `proc_macro::TokenStream` boundary — they must stay one-line
//! forwarders. Add new logic to `rudzio-macro-internals`, never here.

use proc_macro::TokenStream;

use rudzio_macro_internals::{main_codegen, proc_macro_env_codegen, suite_codegen};
use syn::Error;

#[inline]
#[proc_macro_attribute]
pub fn main(args: TokenStream, input: TokenStream) -> TokenStream {
    main_codegen::expand(args.into(), input.into())
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[inline]
#[proc_macro_attribute]
pub fn suite(args: TokenStream, input: TokenStream) -> TokenStream {
    suite_codegen::expand_entry(args.into(), input.into())
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[inline]
#[proc_macro_attribute]
pub fn test(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Internal helper: reads the env var named by `input` (a string
/// literal) via `std::env::var` at expansion time and emits the value
/// as a string literal. Used by rudzio's own tests to verify that
/// `cargo:rustc-env=CARGO_MANIFEST_DIR=<override>` directives in a
/// bridge `build.rs` reach proc-macros (which read env via
/// `std::env::var` rather than the `env!` mechanism that bakes values
/// in at rustc compile time).
///
/// Not a stability guarantee — `#[doc(hidden)]` and underscore-prefixed.
#[doc(hidden)]
#[inline]
#[proc_macro]
pub fn __proc_macro_env(input: TokenStream) -> TokenStream {
    proc_macro_env_codegen::expand(input.into())
        .unwrap_or_else(Error::into_compile_error)
        .into()
}
