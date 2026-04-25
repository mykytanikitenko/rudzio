pub fn b_answer() -> i32 {
    20
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
    fn b_works() {
        assert_eq!(b_answer(), 20);
    }
    */
    #[::rudzio::test]
    async fn b_works(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(b_answer(), 20);
        ::core::result::Result::Ok(())
    }
}
