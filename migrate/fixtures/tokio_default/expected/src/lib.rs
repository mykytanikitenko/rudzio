pub async fn delay() {
    tokio::task::yield_now().await;
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
    #[tokio::test]
    async fn delays_cleanly() {
        delay().await;
    }
    */
    #[::rudzio::test]
    async fn delays_cleanly(_ctx: &Test) -> ::anyhow::Result<()> {
        delay().await;
        ::core::result::Result::Ok(())
    }
}
