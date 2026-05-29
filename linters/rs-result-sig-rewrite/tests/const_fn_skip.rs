#[path = "common/mod.rs"]
mod common;

#[test]
fn const_fn_skip() {
    let outcome = common::run_case(
        include_str!("fixtures/const_fn_skip_input.rs"),
        include_str!("fixtures/const_fn_skip_expected.rs"),
        3,
    );
    assert_eq!(outcome.transformed, 0);
    assert!(
        outcome.logs.iter().any(|l| l.contains("const fn answer")),
        "expected const fn skip log, got {:?}",
        outcome.logs,
    );
}
