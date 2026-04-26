use tests_only_flag::ident;
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
    fn integration_passes() {
        assert_eq!(ident("hi"), "hi");
    }
    */
    #[::rudzio::test]
    async fn integration_passes() {
        assert_eq!(ident("hi"), "hi");
    }
}
#[::rudzio::main]
fn main() {}
