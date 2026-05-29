#[path = "common/mod.rs"]
mod common;

#[test]
fn iterator_next_skip() {
    let outcome = common::run_case(
        include_str!("fixtures/iterator_next_skip_input.rs"),
        include_str!("fixtures/iterator_next_skip_expected.rs"),
    );
    assert_eq!(outcome.transformed, 0);
    assert!(
        outcome.logs.iter().any(|l| l.contains("Iterator::next")),
        "expected Iterator::next skip log, got {:?}",
        outcome.logs,
    );
}
