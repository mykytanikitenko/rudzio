#[path = "common/mod.rs"]
mod common;

#[test]
fn impl_trait() {
    let outcome = common::run_case(
        include_str!("fixtures/impl_trait_input.rs"),
        include_str!("fixtures/impl_trait_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 1);
}
