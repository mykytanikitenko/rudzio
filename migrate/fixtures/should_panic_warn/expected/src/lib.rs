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
    use ::rudzio::common::context::Test;
    /* pre-migration (rudzio-migrate):
    #[test]
    #[should_panic]
    fn panics_as_expected() {
        panic!("expected");
    }
    */
    #[::rudzio::test]
    async fn panics_as_expected(_ctx: &Test) -> ::anyhow::Result<()> {
        panic!("expected");
        ::core::result::Result::Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn ordinary_ok() {
        assert!(true);
    }
    */
    #[::rudzio::test]
    async fn ordinary_ok(_ctx: &Test) -> ::anyhow::Result<()> {
        assert!(true);
        ::core::result::Result::Ok(())
    }
}
