use integration_file::greet;
fn expected_prefix() -> &'static str {
    "hello, "
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
    #[tokio::test]
    async fn greets_alice() {
        let out = greet("alice");
        assert!(out.starts_with(expected_prefix()));
        assert!(out.contains("alice"));
    }
    */
    #[::rudzio::test]
    async fn greets_alice() {
        let out = greet("alice");
        assert!(out.starts_with(expected_prefix()));
        assert!(out.contains("alice"));
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn greets_bob_synchronously() {
        assert_eq!(greet("bob"), "hello, bob");
    }
    */
    #[::rudzio::test]
    async fn greets_bob_synchronously() {
        assert_eq!(greet("bob"), "hello, bob");
    }
}
#[::rudzio::main]
fn main() {}
