pub fn answer() -> i32 {
    42
}
#[cfg(any(test, rudzio_test))]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::compio::Compio::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
mod tests {
    use super::*;
    /* pre-migration (rudzio-migrate):
    #[compio::test]
    async fn runs_under_compio() {
        assert_eq!(answer(), 42);
    }
    */
    #[::rudzio::test]
    async fn runs_under_compio() -> ::anyhow::Result<()> {
        assert_eq!(answer(), 42);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
