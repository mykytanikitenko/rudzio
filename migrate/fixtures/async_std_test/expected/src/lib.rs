pub async fn hello() -> &'static str {
    "world"
}
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
    #[async_std::test]
    async fn greets() {
        assert_eq!(hello().await, "world");
    }
    */
    #[::rudzio::test]
    async fn greets(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(hello(). await, "world");
        ::core::result::Result::Ok(())
    }
}
