//! Proc-macro entry points. All parsing, transformation, and codegen live
//! in [`rudzio_macro_internals`]; this crate is only the `proc-macro = true`
//! wrapper that crosses the `proc_macro::TokenStream` boundary.

use proc_macro::TokenStream;

use rudzio_macro_internals::args::MainArgs;
use rudzio_macro_internals::suite_codegen::expand_suite;

#[proc_macro_attribute]
pub fn main(args: TokenStream, input: TokenStream) -> TokenStream {
    if !args.is_empty() {
        let span = proc_macro2::TokenStream::from(args)
            .into_iter()
            .next()
            .map_or_else(proc_macro2::Span::call_site, |t| t.span());
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

    let _unused: syn::ItemFn = match syn::parse(input) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };
    quote::quote! {
        fn main() {
            // `cargo_meta!()` expands to `env!(CARGO_MANIFEST_DIR)` etc.
            // at THIS call site (the user's crate), so `manifest_dir`
            // resolves to the user's package, not to rudzio's.
            ::rudzio::run(::rudzio::cargo_meta!());
        }
    }
    .into()
}

#[proc_macro_attribute]
pub fn suite(args: TokenStream, input: TokenStream) -> TokenStream {
    let args: MainArgs = match syn::parse(args) {
        Ok(args) => args,
        Err(e) => return e.to_compile_error().into(),
    };

    let input_mod: syn::ItemMod = match syn::parse(input) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };

    match expand_suite(args, input_mod) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn test(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}
