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
    /* pre-migration (rudzio-migrate):
    #[test]
    fn addition_works() {
        assert_eq!(1 + 1, 2);
    }
    */
    #[::rudzio::test]
    async fn addition_works(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(1 + 1, 2);
        ::core::result::Result::Ok(())
    }
    #[bench]
    fn addition_bench(b: &mut test::Bencher) {
        b.iter(|| 1 + 1);
    }
}
