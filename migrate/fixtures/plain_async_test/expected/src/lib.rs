pub fn identity<T>(v: T) -> T {
    v
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
    #[test]
    async fn already_async() {
        let n = identity(7);
        assert_eq!(n, 7);
    }
    */
    #[::rudzio::test]
    async fn already_async(_ctx: &Test) -> ::anyhow::Result<()> {
        let n = identity(7);
        assert_eq!(n, 7);
        ::core::result::Result::Ok(())
    }
}
