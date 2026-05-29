#[path = "common/mod.rs"]
mod common;

#[test]
fn unit_return() {
    let outcome = common::run_case(
        include_str!("fixtures/unit_return_input.rs"),
        include_str!("fixtures/unit_return_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 1);
}
