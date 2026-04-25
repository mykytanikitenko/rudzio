//! Reproducer for the file-v3 symptom: a src file carries BOTH an
//! ambassador-style bodyless impl fn (syn parses it as
//! `ImplItem::Verbatim(TokenStream)`, which prettyplease panics on)
//! AND a `#[cfg(test)] mod tests` with real tests. The tool must
//! preserve the verbatim impl untouched and still migrate the tests.
pub struct MockGenerator;
#[allow(dead_code)]
impl std::sync::Arc<MockGenerator> {
    fn as_ref(&self) -> &MockGenerator;
}
pub fn add(a: i32, b: i32) -> i32 {
    a + b
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
    fn sums_correctly() {
        assert_eq!(add(1, 2), 3);
    }
    */
    #[::rudzio::test]
    async fn sums_correctly() {
        assert_eq!(add(1, 2), 3);
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn sums_zero() {
        assert_eq!(add(0, 0), 0);
    }
    */
    #[::rudzio::test]
    async fn sums_zero() {
        assert_eq!(add(0, 0), 0);
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
