pub fn answer() -> i32 {
    42
}
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::compio::Compio::new,
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
    #[compio::test]
    async fn runs_under_compio() {
        assert_eq!(answer(), 42);
    }
    */
    #[::rudzio::test]
    async fn runs_under_compio(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(answer(), 42);
        ::core::result::Result::Ok(())
    }
}
