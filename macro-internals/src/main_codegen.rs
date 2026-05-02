//! Codegen for the `#[rudzio::main]` attribute.
//!
//! The `rudzio-macro` crate's `main` proc-macro entry point is a thin
//! forwarder that calls [`expand`]; all parsing, validation, and token
//! generation lives here so it's reachable from integration tests
//! (which cannot import items from a `proc-macro = true` crate).

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::ItemFn;

/// Generate the `fn main()` wrapper around a user-supplied
/// `#[rudzio::main]`-annotated function body.
///
/// User-supplied init code runs first (e.g. the per-member manifest-dir
/// registry the cargo-rudzio aggregator installs) so it lands before the
/// runner spins up. `cargo_meta!()` expands to `env!(CARGO_MANIFEST_DIR)`
/// at the user's call site (their crate), so `manifest_dir` resolves to
/// the user's package — not rudzio's.
///
/// # Errors
///
/// - Any tokens passed as arguments to `#[rudzio::main]` (the attribute
///   no longer accepts inline configuration; use
///   `#[rudzio::suite([...])] mod ...` for runtime config).
/// - `input` does not parse as an `ItemFn`.
#[inline]
pub fn expand(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    if !args.is_empty() {
        let span = args
            .into_iter()
            .next()
            .map_or_else(Span::call_site, |token| token.span());
        return Err(syn::Error::new(
            span,
            "`#[rudzio::main]` no longer accepts inline configuration; use \
             `#[rudzio::suite([...])] mod ... { ... }` for each runtime config \
             and a separate `#[rudzio::main] fn main() {}` to install the \
             runner",
        ));
    }

    let func: ItemFn = syn::parse2(input)?;
    let body = &func.block;
    Ok(quote! {
        fn main() -> ::std::process::ExitCode {
            #body
            ::rudzio::run(::rudzio::cargo_meta!())
        }
    })
}
