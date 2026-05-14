//! Signature-transform tests, dogfooded on rudzio.

use syn::{ItemFn, parse_quote};

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
use rudzio_macro_internals::transform::apply_runtime_generics;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{ItemFn, Test, apply_runtime_generics, parse_quote};

    #[rudzio::test]
    fn test_transform_simple_base_context(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: &BaseTestContext) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = apply_runtime_generics(func);

        anyhow::ensure!(!transformed.sig.generics.params.is_empty());

        let first = transformed
            .sig
            .inputs
            .first()
            .ok_or_else(|| anyhow::anyhow!("transformed fn has no inputs"))?;
        let syn::FnArg::Typed(pat_type) = first else {
            anyhow::bail!("expected Typed arg, got {first:?}");
        };
        let syn::Type::Reference(type_ref) = &*pat_type.ty else {
            anyhow::bail!("expected Reference type");
        };
        anyhow::ensure!(
            type_ref.lifetime.is_some(),
            "transformed reference lost its lifetime annotation"
        );
        let syn::Type::Path(type_path) = &*type_ref.elem else {
            anyhow::bail!("expected TypePath");
        };
        let last_seg = type_path
            .path
            .segments
            .last()
            .ok_or_else(|| anyhow::anyhow!("type path has no segments"))?;
        anyhow::ensure!(
            !last_seg.arguments.is_empty(),
            "transformed context type should have been given generic arguments"
        );
        Ok(())
    }

    #[rudzio::test]
    fn test_skip_already_generic_context(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: &PostgresTestContext<'_, TokioMultiThread, MT_RUNTIME, NO_TLS>) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = apply_runtime_generics(func);

        anyhow::ensure!(
            transformed.sig.generics.params.is_empty(),
            "already-generic context must not be transformed"
        );

        let first = transformed
            .sig
            .inputs
            .first()
            .ok_or_else(|| anyhow::anyhow!("fn has no inputs"))?;
        let syn::FnArg::Typed(pat_type) = first else {
            anyhow::bail!("expected Typed arg");
        };
        let syn::Type::Reference(type_ref) = &*pat_type.ty else {
            anyhow::bail!("expected Reference type");
        };
        let syn::Type::Path(type_path) = &*type_ref.elem else {
            anyhow::bail!("expected TypePath");
        };
        let last_seg = type_path
            .path
            .segments
            .last()
            .ok_or_else(|| anyhow::anyhow!("type path has no segments"))?;
        anyhow::ensure!(!last_seg.arguments.is_empty());
        let original: syn::TypePath =
            parse_quote!(PostgresTestContext<'_, TokioMultiThread, MT_RUNTIME, NO_TLS>);
        anyhow::ensure!(type_path.path.segments.len() == original.path.segments.len());
        Ok(())
    }

    #[rudzio::test]
    fn test_transform_custom_context(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: &MyCustomContext) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = apply_runtime_generics(func);

        anyhow::ensure!(!transformed.sig.generics.params.is_empty());

        let first = transformed
            .sig
            .inputs
            .first()
            .ok_or_else(|| anyhow::anyhow!("fn has no inputs"))?;
        let syn::FnArg::Typed(pat_type) = first else {
            anyhow::bail!("expected Typed arg");
        };
        let syn::Type::Reference(type_ref) = &*pat_type.ty else {
            anyhow::bail!("expected Reference type");
        };
        let syn::Type::Path(type_path) = &*type_ref.elem else {
            anyhow::bail!("expected TypePath");
        };
        let last_seg = type_path
            .path
            .segments
            .last()
            .ok_or_else(|| anyhow::anyhow!("type path has no segments"))?;
        anyhow::ensure!(last_seg.ident == "MyCustomContext");
        anyhow::ensure!(!last_seg.arguments.is_empty());
        Ok(())
    }

    #[rudzio::test]
    fn test_no_transform_non_reference_param(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: BaseTestContext) -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = apply_runtime_generics(func);

        anyhow::ensure!(
            transformed.sig.generics.params.is_empty(),
            "non-reference param must not get generic lifetimes injected"
        );
        Ok(())
    }

    #[rudzio::test]
    fn test_no_transform_no_params(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn test() -> anyhow::Result<()> {
                Ok(())
            }
        };

        let transformed = apply_runtime_generics(func);

        anyhow::ensure!(
            transformed.sig.generics.params.is_empty(),
            "no-params fn must not get generic lifetimes injected"
        );
        Ok(())
    }
}
