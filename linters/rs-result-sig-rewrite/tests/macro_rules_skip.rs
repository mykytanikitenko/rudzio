#[path = "common/mod.rs"]
mod common;

#[test]
fn macro_rules_skip() {
    let outcome = common::run_case(
        include_str!("fixtures/macro_rules_skip_input.rs"),
        include_str!("fixtures/macro_rules_skip_expected.rs"),
    );
    assert_eq!(outcome.transformed, 0);
}
