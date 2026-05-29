#[path = "common/mod.rs"]
mod common;

#[test]
fn intra_file_callsite() {
    let outcome = common::run_case(
        include_str!("fixtures/intra_file_callsite_input.rs"),
        include_str!("fixtures/intra_file_callsite_expected.rs"),
    );
    assert_eq!(outcome.transformed, 2);
    assert_eq!(outcome.fixme_callsites, 1);
}
