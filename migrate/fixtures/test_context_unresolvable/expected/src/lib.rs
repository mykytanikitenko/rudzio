pub struct ExternalCtx;
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
#[cfg(test)]
mod tests {
    use ::rudzio::common::context::Test;
    use super::*;
    /* pre-migration (rudzio-migrate):
    #[test_context(ExternalCtx)]
    #[tokio::test]
    async fn uses_external(ctx: &mut ExternalCtx) {
        let _ = ctx;
    }
    */
    #[::rudzio::test]
    async fn uses_external(ctx: &mut ExternalCtx) -> ::anyhow::Result<()> {
        let _ = ctx;
        ::core::result::Result::Ok(())
    }
}
