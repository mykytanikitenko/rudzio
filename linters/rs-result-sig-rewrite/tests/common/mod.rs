//! Shared snapshot harness for the rs-result-sig-rewrite integration
//! tests. Each test file pulls this in via `#[path]` so cargo does
//! not promote it to its own test crate.

use rs_result_sig_rewrite::{TransformOutcome, transform};

/// Run `transform` on `input` and assert the result equals
/// `expected` byte-for-byte. On mismatch, prints both sides for
/// inspection. Returns the outcome so the caller can additionally
/// assert on exit code / log counts when relevant.
pub fn run_case(input: &str, expected: &str) -> TransformOutcome {
    let outcome = match transform(input) {
        Ok(o) => o,
        Err(e) => panic!("transform errored: {e}"),
    };
    if outcome.new_source != expected {
        eprintln!("--- expected ({} bytes) ---", expected.len());
        eprintln!("{expected}");
        eprintln!("--- actual ({} bytes) ---", outcome.new_source.len());
        eprintln!("{out}", out = outcome.new_source);
        eprintln!("--- end ---");
        panic!("transform output did not match expected fixture");
    }
    outcome
}

/// Same as [`run_case`] but additionally asserts the suggested
/// process exit code.
pub fn run_case_with_exit(input: &str, expected: &str, expected_exit: u8) -> TransformOutcome {
    let outcome = run_case(input, expected);
    assert_eq!(
        outcome.exit_code, expected_exit,
        "exit code mismatch: got {got}, expected {want}",
        got = outcome.exit_code,
        want = expected_exit,
    );
    outcome
}
