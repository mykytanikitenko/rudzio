pub fn prefix_matches(haystack: &str, needle: &str) -> bool {
    haystack.starts_with(needle)
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
    use ::rudzio::common::context::Test;
    use super::*;
    use rstest::rstest;
    /* pre-migration (rudzio-migrate):
    #[test]
    fn baseline_works() {
        assert!(prefix_matches("hello", "he"));
    }
    */
    #[::rudzio::test]
    async fn baseline_works(_ctx: &Test) -> ::anyhow::Result<()> {
        assert!(prefix_matches("hello", "he"));
        ::core::result::Result::Ok(())
    }
    #[rstest]
    #[case("x", "x")]
    #[case("xy", "x")]
    #[case("xy", "y")]
    fn matches_cases(#[case] haystack: &str, #[case] needle: &str) {
        assert!(prefix_matches(haystack, needle) || haystack.contains(needle));
    }
}
