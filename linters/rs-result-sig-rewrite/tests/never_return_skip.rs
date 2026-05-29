#[path = "common/mod.rs"]
mod common;

#[test]
fn never_return_skip() {
    let outcome = common::run_case(
        include_str!("fixtures/never_return_skip_input.rs"),
        include_str!("fixtures/never_return_skip_expected.rs"),
    );
    assert_eq!(outcome.transformed, 0);
    assert!(
        outcome.logs.iter().any(|l| l.contains("never-return fn forever")),
        "expected never-return skip log, got {:?}",
        outcome.logs,
    );
}
