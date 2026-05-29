#[path = "common/mod.rs"]
mod common;

#[test]
fn existing_question_mark() {
    let outcome = common::run_case(
        include_str!("fixtures/existing_question_mark_input.rs"),
        include_str!("fixtures/existing_question_mark_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 1);
}
