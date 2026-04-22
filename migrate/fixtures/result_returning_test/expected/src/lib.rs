pub fn parse_u8(s: &str) -> Result<u8, std::num::ParseIntError> {
    s.parse()
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
    fn parses_cleanly() -> Result<(), std::num::ParseIntError> {
        let n = parse_u8("42")?;
        assert_eq!(n, 42);
        Ok(())
    }
    */
    #[::rudzio::test]
    async fn parses_cleanly(_ctx: &Test) -> Result<(), std::num::ParseIntError> {
        let n = parse_u8("42")?;
        assert_eq!(n, 42);
        Ok(())
    }
}
