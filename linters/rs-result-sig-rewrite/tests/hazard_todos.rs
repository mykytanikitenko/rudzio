#[path = "common/mod.rs"]
mod common;

#[test]
fn hazard_todos() {
    let outcome = common::run_case(
        include_str!("fixtures/hazard_todos_input.rs"),
        include_str!("fixtures/hazard_todos_expected.rs"),
    );
    assert_eq!(outcome.transformed, 1);
    assert_eq!(outcome.hazard_todos, 1);
}
