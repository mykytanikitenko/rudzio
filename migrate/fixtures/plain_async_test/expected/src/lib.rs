pub fn identity<T>(v: T) -> T {
    v
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
    #[test]
    async fn already_async() {
        let n = identity(7);
        assert_eq!(n, 7);
    }
    */
    #[::rudzio::test]
    async fn already_async() -> ::anyhow::Result<()> {
        let n = identity(7);
        assert_eq!(n, 7);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
