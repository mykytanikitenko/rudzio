pub fn a_answer() -> i32 {
    10
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
    #[test]
    fn a_works() {
        assert_eq!(a_answer(), 10);
    }
    */
    #[::rudzio::test]
    async fn a_works() -> ::anyhow::Result<()> {
        assert_eq!(a_answer(), 10);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
