pub fn parse_u8(s: &str) -> Result<u8, std::num::ParseIntError> {
    s.parse()
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
    /* pre-migration (rudzio-migrate):
    #[test]
    fn parses_via_block_body() -> Result<(), std::num::ParseIntError> {
        {
            let n = parse_u8("42")?;
            assert_eq!(n, 42);
            Ok(())
        }
    }
    */
    #[::rudzio::test]
    async fn parses_via_block_body(_ctx: &Test) -> Result<(), std::num::ParseIntError> {
        {
            let n = parse_u8("42")?;
            assert_eq!(n, 42);
            Ok(())
        }
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
