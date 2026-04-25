//! A minimal reproduction of the `ambassador::delegate_to_remote_methods`
//! shape: an `impl` block with a bodyless fn signature. syn parses
//! the item as `ImplItem::Verbatim(TokenStream)` as a fallback;
//! prettyplease::unparse then panics when it reaches it.
//!
//! The tool's `catch_unwind` around `prettyplease::unparse` must turn
//! that into a specific warning and leave this file untouched
//! (post-migration content byte-identical to pre-migration).
pub struct MockGenerator;
#[allow(dead_code)]
impl std::sync::Arc<MockGenerator> {
    fn as_ref(&self) -> &MockGenerator;
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
    /* pre-migration (rudzio-migrate):
    #[test]
    fn trivial() {
        assert!(true);
    }
    */
    #[::rudzio::test]
    async fn trivial() -> ::anyhow::Result<()> {
        assert!(true);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
