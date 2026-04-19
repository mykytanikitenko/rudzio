use proc_macro::TokenStream;

mod args;
mod codegen;
mod suite_codegen;
mod transform;

#[proc_macro_attribute]
pub fn main(args: TokenStream, input: TokenStream) -> TokenStream {
    if !args.is_empty() {
        return syn::Error::new(
            proc_macro2::TokenStream::from(args).into_iter().next().map_or_else(
                proc_macro2::Span::call_site,
                |t| t.span(),
            ),
            "`#[rudzio::main]` no longer accepts inline configuration; use \
             `#[rudzio::suite([...])] mod ... { ... }` for each runtime config \
             and a separate `#[rudzio::main] fn main() {}` to install the \
             runner",
        )
        .to_compile_error()
        .into();
    }

    let _: syn::ItemFn = match syn::parse(input) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };
    quote::quote! {
        fn main() {
            ::rudzio::run();
        }
    }
    .into()
}

#[proc_macro_attribute]
pub fn suite(args: TokenStream, input: TokenStream) -> TokenStream {
    let args: args::MainArgs = match syn::parse(args) {
        Ok(args) => args,
        Err(e) => return e.to_compile_error().into(),
    };

    let input_mod: syn::ItemMod = match syn::parse(input) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };

    suite_codegen::expand_suite(args, input_mod)
}

#[proc_macro_attribute]
pub fn test(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}
