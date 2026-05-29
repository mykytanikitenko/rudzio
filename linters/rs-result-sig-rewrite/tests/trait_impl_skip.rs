#[path = "common/mod.rs"]
mod common;

#[test]
fn trait_impl_skip() {
    let outcome = common::run_case(
        include_str!("fixtures/trait_impl_skip_input.rs"),
        include_str!("fixtures/trait_impl_skip_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 0);
    assert!(outcome.skipped >= 1, "expected at least one skip log, got {}", outcome.skipped);
    assert!(
        outcome.logs.iter().any(|l| l.contains("PartialEq::eq")),
        "expected a PartialEq::eq skip log, got {:?}",
        outcome.logs,
    );
}
