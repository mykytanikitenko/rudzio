pub async fn hello() -> &'static str {
    "world"
}
#[cfg(test)]
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
    #[async_std::test]
    async fn greets() {
        assert_eq!(hello().await, "world");
    }
    */
    #[::rudzio::test]
    async fn greets() -> ::anyhow::Result<()> {
        assert_eq!(hello(). await, "world");
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
