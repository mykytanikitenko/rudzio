#[path = "common/mod.rs"]
mod common;

#[test]
fn async_fn() {
    let outcome = common::run_case(
        include_str!("fixtures/async_fn_input.rs"),
        include_str!("fixtures/async_fn_expected.rs"),
        0,
    );
    assert_eq!(outcome.transformed, 1);
    assert_eq!(outcome.exit_code, 0);
}
