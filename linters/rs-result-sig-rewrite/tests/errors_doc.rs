//! T-1: a converted bare-`pub` fn gets a `# Errors` doc stub (so
//! `clippy::missing_errors_doc`, forbidden workspace-wide, passes).
//! A private fn does NOT (the lint only fires on exported items).

#[path = "common/mod.rs"]
mod common;

#[test]
fn errors_doc_added_to_pub_only() {
    let outcome = common::run_case(
        include_str!("fixtures/errors_doc_input.rs"),
        include_str!("fixtures/errors_doc_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 2);
    // Only the `pub fn` gets a `# Errors` stub.
    assert_eq!(outcome.errors_docs, 1);
}
