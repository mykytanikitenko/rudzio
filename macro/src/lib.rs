use proc_macro::TokenStream;

mod args;
mod codegen;
mod suite_codegen;
mod transform;

#[proc_macro_attribute]
pub fn main(args: TokenStream, input: TokenStream) -> TokenStream {
    // If args are provided, delegate to the legacy expand_main for backwards
    // compatibility. Otherwise emit a minimal fn main that calls rudzio::run().
    if args.is_empty() {
        // Expect `fn main() {}` as input and replace it with the rudzio runner.
        let _: syn::ItemFn = match syn::parse(input) {
            Ok(f) => f,
            Err(e) => return e.to_compile_error().into(),
        };
        return quote::quote! {
            fn main() {
                ::rudzio::run();
            }
        }
        .into();
    }

    let args: args::MainArgs = match syn::parse(args) {
        Ok(args) => args,
        Err(e) => return e.to_compile_error().into(),
    };

    let input_mod: syn::ItemMod = match syn::parse(input) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };

    codegen::expand_main(args, input_mod)
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
