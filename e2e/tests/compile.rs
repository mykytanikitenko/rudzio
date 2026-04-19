//! Compile-level assertions for the rudzio macro.
//!
//! Currently contains one failing expectation: a sync (non-`async`) test body
//! should be accepted by `#[rudzio::main]` but the generated `spawn_blocking`
//! arm does not compile.

#[test]
fn sync_test_should_compile() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/fixtures/sync_test.rs");
}
