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
    #[ignore]
    fn bare_ignore() {}
    */
    #[::rudzio::test]
    #[ignore]
    async fn bare_ignore() {}
    /* pre-migration (rudzio-migrate):
    #[test]
    #[ignore = "reason as name-value"]
    fn named_ignore() {}
    */
    #[::rudzio::test]
    #[ignore = "reason as name-value"]
    async fn named_ignore() {}
    /* pre-migration (rudzio-migrate):
    #[test]
    #[ignore("reason as list-literal")]
    fn list_literal_ignore() {}
    */
    #[::rudzio::test]
    #[ignore("reason as list-literal")]
    async fn list_literal_ignore() {}
    /* pre-migration (rudzio-migrate):
    #[test]
    fn not_ignored() {}
    */
    #[::rudzio::test]
    async fn not_ignored() {}
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
