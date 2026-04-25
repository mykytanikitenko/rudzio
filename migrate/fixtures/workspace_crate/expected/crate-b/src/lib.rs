pub fn b_answer() -> i32 {
    20
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
    fn b_works() {
        assert_eq!(b_answer(), 20);
    }
    */
    #[::rudzio::test]
    async fn b_works() -> ::anyhow::Result<()> {
        assert_eq!(b_answer(), 20);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
