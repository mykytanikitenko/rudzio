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

        // Inline trait bounds must not be emitted: `clippy::inline_trait_bounds`
        // denies them. Both the lifetime outlives bound and the `R` trait
        // bounds belong in the `where` clause.
        let params = &transformed.sig.generics.params;
        anyhow::ensure!(
            params.len() == 3,
            "expected exactly 3 generic params, got {}",
            params.len()
        );
        for param in params {
            let bounds_empty = match param {
                syn::GenericParam::Lifetime(lp) => lp.bounds.is_empty(),
                syn::GenericParam::Type(tp) => tp.bounds.is_empty(),
                syn::GenericParam::Const(_) => true,
            };
            anyhow::ensure!(
                bounds_empty,
                "generic param carries inline bounds: {param:?}"
            );
        }

        let where_clause = transformed
            .sig
            .generics
            .where_clause
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected a where clause"))?;
        anyhow::ensure!(
            where_clause.predicates.len() == 2,
            "expected 2 where predicates, got {}",
            where_clause.predicates.len()
        );

        let mut predicates = where_clause.predicates.iter();
        let syn::WherePredicate::Lifetime(pred_lifetime) = predicates
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing lifetime predicate"))?
        else {
            anyhow::bail!("first predicate should be a lifetime bound");
        };
        anyhow::ensure!(pred_lifetime.lifetime.ident == "suite_context");
        anyhow::ensure!(pred_lifetime.bounds.len() == 1);
        let test_context_bound = pred_lifetime
            .bounds
            .first()
            .ok_or_else(|| anyhow::anyhow!("expected one lifetime bound"))?;
        anyhow::ensure!(test_context_bound.ident == "test_context");

        let syn::WherePredicate::Type(pred_type) = predicates
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing type predicate"))?
        else {
            anyhow::bail!("second predicate should be a type bound");
        };
        let syn::Type::Path(bounded_path) = &pred_type.bounded_ty else {
            anyhow::bail!("expected R as a path type");
        };
        anyhow::ensure!(bounded_path.path.is_ident("R"));
        anyhow::ensure!(
            pred_type.bounds.len() == 2,
            "expected 2 trait bounds on R (Runtime + Sync), got {}",
            pred_type.bounds.len()
        );
        anyhow::ensure!(
            predicates.next().is_none(),
            "unexpected extra where predicates"
        );

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
    fn test_merges_existing_where_clause(_ctx: &Test) -> anyhow::Result<()> {
        let func: ItemFn = parse_quote! {
            async fn test(ctx: &BaseTestContext) -> anyhow::Result<()>
            where
                BaseTestContext: Send,
            {
                Ok(())
            }
        };

        let transformed = apply_runtime_generics(func);

        let where_clause = transformed
            .sig
            .generics
            .where_clause
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected a where clause"))?;
        anyhow::ensure!(
            where_clause.predicates.len() == 3,
            "expected the user predicate plus the two injected predicates, got {}",
            where_clause.predicates.len()
        );

        // The user's original predicate is preserved untouched as the first.
        let first = where_clause
            .predicates
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing first predicate"))?;
        anyhow::ensure!(matches!(
            first,
            syn::WherePredicate::Type(pred)
                if matches!(&pred.bounded_ty, syn::Type::Path(path) if path.path.is_ident("BaseTestContext"))
        ));

        // The injected lifetime and `R` bounds follow it.
        let injected: Vec<_> = where_clause.predicates.iter().skip(1).collect();
        anyhow::ensure!(injected.len() == 2);
        anyhow::ensure!(matches!(
            injected.first(),
            Some(syn::WherePredicate::Lifetime(pred)) if pred.lifetime.ident == "suite_context"
        ));
        anyhow::ensure!(matches!(
            injected.get(1),
            Some(syn::WherePredicate::Type(pred))
                if matches!(&pred.bounded_ty, syn::Type::Path(path) if path.path.is_ident("R"))
        ));
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
