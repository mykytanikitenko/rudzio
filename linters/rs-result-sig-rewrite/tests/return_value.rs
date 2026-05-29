#[path = "common/mod.rs"]
mod common;

#[test]
fn return_value() {
    let outcome = common::run_case(
        include_str!("fixtures/return_value_input.rs"),
        include_str!("fixtures/return_value_expected.rs"),
    );
    assert_eq!(outcome.transformed, 1);
}
