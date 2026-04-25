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

#[cfg(test)]
mod tests {
    #[test]
    fn trivial() {
        assert!(true);
    }
}
