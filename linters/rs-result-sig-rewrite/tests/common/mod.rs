//! Shared snapshot harness for the rs-result-sig-rewrite integration
//! tests. Each test file pulls this in via `mod common;` so cargo
//! does not promote it to its own test crate. Single entry point —
//! splitting `run_case` from `run_case_with_exit` would make one of
//! them dead-code in every test binary that doesn't call it, and the
//! workspace forbids `#[allow(dead_code)]`.

use rs_result_sig_rewrite::{TransformOutcome, transform};

/// Run `transform` on `input` and assert the result equals `expected`
/// byte-for-byte AND that the suggested process exit code matches
/// `expected_exit`. On either mismatch, prints both sides for
/// inspection then panics. Returns the outcome so callers can layer
/// additional assertions (log counts, FIXME markers, etc.).
pub fn run_case(input: &str, expected: &str, expected_exit: u8) -> TransformOutcome {
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
    assert_eq!(
        outcome.exit_code, expected_exit,
        "exit code mismatch: got {got}, expected {want}",
        got = outcome.exit_code,
        want = expected_exit,
    );
    outcome
}
