pub struct ExternalCtx;
#[cfg(any(test, rudzio_test))]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
mod tests {
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
#[cfg(test)]
#[::rudzio::main]
fn main() {}
