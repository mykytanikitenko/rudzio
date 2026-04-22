pub mod ctx;
pub fn zero() -> u32 {
    0
}
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = crate::ctx::DeepCtxRudzioSuite,
        test = crate::ctx::DeepCtxRudzioBridge,
    ),
    ]
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::DeepCtx;
    /* pre-migration (rudzio-migrate):
    #[test_context(DeepCtx)]
    #[tokio::test]
    async fn uses_deep_ctx(ctx: &mut DeepCtx) {
        assert_eq!(ctx.depth, zero());
    }
    */
    #[::rudzio::test]
    async fn uses_deep_ctx(ctx: &mut DeepCtxRudzioBridge) -> ::anyhow::Result<()> {
        assert_eq!(ctx.depth, zero());
        ::core::result::Result::Ok(())
    }
}
