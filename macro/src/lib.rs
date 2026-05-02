//! Proc-macro entry points. All parsing, transformation, and codegen live
//! in [`rudzio_macro_internals`]; this crate is only the `proc-macro = true`
//! wrapper that crosses the `proc_macro::TokenStream` boundary.

use proc_macro::TokenStream;

use proc_macro2::Span;
use proc_macro2::TokenStream as TokenStream2;
use rudzio_macro_internals::parse::MainArgs;
use rudzio_macro_internals::suite_codegen::expand_suite;

#[inline]
#[proc_macro_attribute]
pub fn main(args: TokenStream, input: TokenStream) -> TokenStream {
    if !args.is_empty() {
        let span = TokenStream2::from(args)
            .into_iter()
            .next()
            .map_or_else(Span::call_site, |token| token.span());
        return syn::Error::new(
            span,
            "`#[rudzio::main]` no longer accepts inline configuration; use \
             `#[rudzio::suite([...])] mod ... { ... }` for each runtime config \
             and a separate `#[rudzio::main] fn main() {}` to install the \
             runner",
        )
        .to_compile_error()
        .into();
    }

    let func: syn::ItemFn = match syn::parse(input) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };
    let body = &func.block;
    quote::quote! {
        fn main() -> ::std::process::ExitCode {
            // User-supplied body runs first, so init code (e.g. the
            // per-member manifest-dir registry the cargo-rudzio
            // aggregator installs) lands before the runner spins up.
            // `cargo_meta!()` expands to `env!(CARGO_MANIFEST_DIR)` etc.
            // at THIS call site (the user's crate), so `manifest_dir`
            // resolves to the user's package, not to rudzio's.
            #body
            ::rudzio::run(::rudzio::cargo_meta!())
        }
    }
    .into()
}

#[inline]
#[proc_macro_attribute]
pub fn suite(args: TokenStream, input: TokenStream) -> TokenStream {
    let parsed_args: MainArgs = match syn::parse(args) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };

    let input_mod: syn::ItemMod = match syn::parse(input) {
        Ok(module) => module,
        Err(err) => return err.to_compile_error().into(),
    };

    match expand_suite(&parsed_args, input_mod) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[inline]
#[proc_macro_attribute]
pub fn test(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}
