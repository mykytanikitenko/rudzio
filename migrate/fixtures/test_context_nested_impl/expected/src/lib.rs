pub mod ctx;
pub fn zero() -> u32 {
    0
}
#[cfg(any(test, rudzio_test))]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = crate::ctx::DeepCtxRudzioSuite,
        test = crate::ctx::DeepCtxRudzioBridge,
    ),
    ]
)]
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
    async fn uses_deep_ctx(ctx: &mut DeepCtxRudzioBridge) {
        assert_eq!(ctx.depth, zero());
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
