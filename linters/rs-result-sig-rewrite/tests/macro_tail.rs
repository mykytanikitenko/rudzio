#[path = "common/mod.rs"]
mod common;

#[test]
fn macro_tail() {
    let outcome = common::run_case(
        include_str!("fixtures/macro_tail_input.rs"),
        include_str!("fixtures/macro_tail_expected.rs"),
    );
    assert_eq!(outcome.transformed, 1);
    assert!(outcome.hazard_todos >= 1, "expected at least one hazard TODO");
}
