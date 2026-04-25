//! Covers every return shape `#[rudzio::test]` accepts. Regression guard
//! for the claim that `#[test]` can be swapped with `#[rudzio::test]`
//! verbatim without the user thinking about `Result`: bare `fn` bodies
//! (no return) and `-> ()` bodies both type-check and run. The trait
//! doing the lifting is `rudzio::IntoRudzioResult` at
//! `src/test_case.rs:IntoRudzioResult`, invoked by the suite codegen at
//! `macro-internals/src/suite_codegen.rs`.

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    // Implicit `()` return, zero args. The shape a user coming from
    // stock libtest writes.
    #[rudzio::test]
    fn bare_sync_void() {
        assert_eq!(1 + 1, 2);
    }

    // Explicit `-> ()` return, zero args.
    #[rudzio::test]
    fn explicit_unit_return() -> () {}

    // Async + implicit `()` return, zero args — the `#[tokio::test]`
    // shape without the macro having to rewrite anything.
    #[rudzio::test]
    async fn bare_async_void() {
        std::future::ready(()).await;
    }

    // Result-returning bodies continue to work — the trait dispatch
    // handles both arms of the `IntoRudzioResult` trait.
    #[rudzio::test]
    async fn result_returning() -> Result<(), std::io::Error> {
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
