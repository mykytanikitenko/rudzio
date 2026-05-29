#[path = "common/mod.rs"]
mod common;

#[test]
fn generic_where_clause() {
    let outcome = common::run_case(
        include_str!("fixtures/generic_where_clause_input.rs"),
        include_str!("fixtures/generic_where_clause_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 1);
}
