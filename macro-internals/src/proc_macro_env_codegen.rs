//! Codegen for the internal `__proc_macro_env!` proc-macro.
//!
//! Reads the env var named by the input string literal via
//! [`std::env::var`] at expansion time and emits the value as a string
//! literal. Used by rudzio's own tests to verify that
//! `cargo:rustc-env=CARGO_MANIFEST_DIR=<override>` directives in a
//! bridge `build.rs` reach proc-macros (which read env via
//! [`std::env::var`] rather than the `env!` mechanism that bakes
//! values in at rustc compile time).

use std::env;

use proc_macro2::TokenStream;
use quote::quote;
use syn::LitStr;

/// Parse `input` as a string literal naming an env var, look it up at
/// expansion time, and emit the value (or empty string when unset) as
/// a `&'static str` literal.
///
/// # Errors
///
/// `input` is not a single string literal.
#[inline]
pub fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let var: LitStr = syn::parse2(input)?;
    let value = env::var(var.value()).unwrap_or_default();
    let lit = LitStr::new(&value, var.span());
    Ok(quote! { #lit })
}
