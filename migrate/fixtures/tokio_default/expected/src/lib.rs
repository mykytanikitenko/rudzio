pub async fn delay() {
    tokio::task::yield_now().await;
}
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
    #[tokio::test]
    async fn delays_cleanly() {
        delay().await;
    }
    */
    #[::rudzio::test]
    async fn delays_cleanly() -> ::anyhow::Result<()> {
        delay().await;
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
