#[path = "common/mod.rs"]
mod common;

#[test]
fn intra_file_callsite() {
    let outcome = common::run_case(
        include_str!("fixtures/intra_file_callsite_input.rs"),
        include_str!("fixtures/intra_file_callsite_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 2);
    // T-2: the call is inside the converted `caller` body, so `?` is
    // inserted (not a FIXME).
    assert_eq!(outcome.question_callsites, 1);
    assert_eq!(outcome.fixme_callsites, 0);
}
