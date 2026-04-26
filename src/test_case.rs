use std::any::Any;
use std::error;
use std::fmt;
use std::pin::Pin;

pub type BoxError = Box<dyn error::Error + Send + Sync + 'static>;

pub type TestFn =
    for<'body> fn(
        &'body mut dyn Any,
    ) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'body>>;

/// Wraps any `Display + Debug` value as a `BoxError`.
/// Used by generated code to support error types (e.g. `anyhow::Error`)
/// that don't implement `std::error::Error` directly.
#[derive(Debug)]
struct DisplayError(String);

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct TestCase {
    pub func: TestFn,
    pub ignore_reason: &'static str,
    pub ignored: bool,
    pub name: &'static str,
}

/// Bridge trait the test macro uses to accept every shape of test-body
/// return value that stock `#[test]` / `#[tokio::test]` accept.
///
/// Implemented for:
/// - `()` (bare-body tests: `#[rudzio::test] fn foo() { assert!(...) }`)
///    — treated as success; any panic inside the body is caught by the
///    runner's `catch_unwind`.
/// - `Result<T, E>` where `E: Display` — success on `Ok(_)`, the error
///    message is extracted on `Err` via [`box_error`]. `T` is discarded.
///
/// The codegen at `macro-internals/src/suite_codegen.rs` emits
/// `<body>.into_rudzio_result()` (or `.await.into_rudzio_result()` for
/// async bodies) so a single dispatch path covers every supported
/// signature shape.
pub trait IntoRudzioResult {
    /// Convert this value into the runner's canonical
    /// `Result<(), BoxError>` form.
    ///
    /// # Errors
    ///
    /// Returns the test body's error boxed into [`BoxError`] when the
    /// implementing type is `Result::Err`; never errors for `()`.
    fn into_rudzio_result(self) -> Result<(), BoxError>;
}

impl error::Error for DisplayError {}

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl IntoRudzioResult for () {
    #[inline]
    fn into_rudzio_result(self) -> Result<(), BoxError> {
        Ok(())
    }
}

impl<T, E: fmt::Display> IntoRudzioResult for Result<T, E> {
    #[inline]
    fn into_rudzio_result(self) -> Result<(), BoxError> {
        self.map(|_| ()).map_err(box_error)
    }
}

impl TestCase {
    #[inline]
    #[must_use]
    pub const fn new(
        func: TestFn,
        ignore_reason: &'static str,
        ignored: bool,
        name: &'static str,
    ) -> Self {
        Self {
            func,
            ignore_reason,
            ignored,
            name,
        }
    }
}

/// Convert any `Display` error into a [`BoxError`].
#[inline]
pub fn box_error<E: fmt::Display>(err: E) -> BoxError {
    Box::new(DisplayError(format!("{err:#}")))
}
