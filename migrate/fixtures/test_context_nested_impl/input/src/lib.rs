pub mod ctx;

pub fn zero() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::DeepCtx;
    use test_context::test_context;

    // The `impl AsyncTestContext for DeepCtx` lives under src/ctx/mod.rs.
    // The suite attr emitted by the tool must reference
    // `crate::ctx::DeepCtxRudzioSuite` / `crate::ctx::DeepCtxRudzioBridge`,
    // not `crate::Deep...` — that's what `infer_module_path` buys.
    #[test_context(DeepCtx)]
    #[tokio::test]
    async fn uses_deep_ctx(ctx: &mut DeepCtx) {
        assert_eq!(ctx.depth, zero());
    }
}
