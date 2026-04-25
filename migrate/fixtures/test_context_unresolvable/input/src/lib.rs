// `ExternalCtx` represents a context type defined in another crate
// whose `impl AsyncTestContext for ExternalCtx` the migration tool
// can't locate by scanning this crate's sources.
pub struct ExternalCtx;

#[cfg(test)]
mod tests {
    use super::*;
    use test_context::test_context;

    #[test_context(ExternalCtx)]
    #[tokio::test]
    async fn uses_external(ctx: &mut ExternalCtx) {
        let _ = ctx;
    }
}
