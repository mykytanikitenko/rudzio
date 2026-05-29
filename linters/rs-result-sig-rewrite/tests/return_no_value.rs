#[path = "common/mod.rs"]
mod common;

#[test]
fn return_no_value() {
    let outcome = common::run_case(
        include_str!("fixtures/return_no_value_input.rs"),
        include_str!("fixtures/return_no_value_expected.rs"),
    );
    assert_eq!(outcome.transformed, 1);
}
