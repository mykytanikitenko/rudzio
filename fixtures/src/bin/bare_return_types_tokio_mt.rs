//! Covers every return shape `#[rudzio::test]` accepts. Regression guard
//! for the claim that `#[test]` can be swapped with `#[rudzio::test]`
//! verbatim without the user thinking about `Result`: bare `fn` bodies
//! (no return) and `-> ()` bodies both type-check and run. The trait
//! doing the lifting is `rudzio::IntoRudzioResult` at
//! `src/test_case.rs:IntoRudzioResult`, invoked by the suite codegen at
//! `macro-internals/src/suite_codegen.rs`.

use std::future::ready;
use std::io::Error as IoError;

use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread;

/// Compute one plus one; the `bare_sync_void` test asserts the result
/// equals two. Wrapping the arithmetic in a function lets the
/// `assert_eq!` arms come from distinct call sites, keeping the
/// `clippy::eq_op` and `clippy::default_numeric_fallback` lints quiet
/// while still exercising the bare-`fn`-with-no-return signature.
const fn one_plus_one() -> u32 {
    1_u32.saturating_add(1_u32)
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::{IoError, one_plus_one, ready};

    // Implicit `()` return, zero args. The shape a user coming from
    // stock libtest writes.
    #[rudzio::test]
    fn bare_sync_void() {
        assert_eq!(one_plus_one(), 2_u32, "1 + 1 must equal 2");
    }

    // Explicit `-> ()` return, zero args.
    #[rudzio::test]
    fn explicit_unit_return() {}

    // Async + implicit `()` return, zero args — the `#[tokio::test]`
    // shape without the macro having to rewrite anything.
    #[rudzio::test]
    async fn bare_async_void() {
        ready(()).await;
    }

    // Result-returning bodies continue to work — the trait dispatch
    // handles both arms of the `IntoRudzioResult` trait.
    #[rudzio::test]
    async fn result_returning() -> Result<(), IoError> {
        Ok(())
    }

    // Result with a Display-but-not-Error type survives through
    // `box_error`. `String` satisfies the Display bound.
    #[rudzio::test]
    async fn result_display_only() -> Result<(), String> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
