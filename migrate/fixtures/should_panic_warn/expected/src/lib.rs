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
    /* pre-migration (rudzio-migrate):
    #[test]
    #[should_panic]
    fn panics_as_expected() {
        panic!("expected");
    }
    */
    #[::rudzio::test]
    async fn panics_as_expected() {
        panic!("expected");
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn ordinary_ok() {
        assert!(true);
    }
    */
    #[::rudzio::test]
    async fn ordinary_ok() {
        assert!(true);
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
