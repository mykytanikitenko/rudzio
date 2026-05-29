#[path = "common/mod.rs"]
mod common;

#[test]
fn tail_side_effects() {
    let outcome = common::run_case(
        include_str!("fixtures/tail_side_effects_input.rs"),
        include_str!("fixtures/tail_side_effects_expected.rs"),
    );
    assert_eq!(outcome.transformed, 1);
}
