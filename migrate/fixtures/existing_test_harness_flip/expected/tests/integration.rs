use existing_test_harness_flip::add;
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
    fn sums() {
        assert_eq!(add(1, 2), 3);
    }
    */
    #[::rudzio::test]
    async fn sums() {
        assert_eq!(add(1, 2), 3);
    }
}
#[::rudzio::main]
fn main() {}
