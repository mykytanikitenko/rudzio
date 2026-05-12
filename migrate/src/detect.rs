//! Test-attribute classification.
//!
//! Given a `syn::Attribute`, decide whether it's a recognised
//! test/runtime macro and, if so, what runtime hint (if any) it
//! carries.

use syn::{Attribute, Lit, Meta};

use crate::cli::RuntimeChoice;

/// Recognised test-attribute kinds. Any unfamiliar attribute is
/// classified as `None` upstream rather than added here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TestKind {
    /// `#[actix_rt::test]` / `#[actix_web::test]`.
    Actix,
    /// `#[async_std::test]`.
    AsyncStd,
    /// `#[compio::test]`.
    Compio,
    /// `#[futures_test::test]`.
    FuturesTest,
    /// Plain `#[test]`. No forced runtime.
    PlainTest,
    /// `#[tokio::test(flavor = "current_thread", ...)]`.
    TokioCurrent,
    /// `#[tokio::test]` without `flavor=` → Tokio multi-thread.
    TokioDefault,
    /// `#[tokio::test(flavor = "multi_thread", ...)]`.
    TokioMulti,
}

impl TestKind {
    /// Forced-runtime mapping: returns `Some(_)` when the attribute
    /// itself dictates the runtime (Tokio variants, Compio); returns
    /// `None` for kinds that fall back to `--runtime default` (plain
    /// `#[test]` and the runtime families rudzio doesn't ship a
    /// dedicated equivalent for in v1).
    #[inline]
    #[must_use]
    pub const fn forced_runtime(self) -> Option<RuntimeChoice> {
        match self {
            Self::Compio => Some(RuntimeChoice::Compio),
            Self::TokioCurrent => Some(RuntimeChoice::TokioCt),
            Self::TokioDefault | Self::TokioMulti => Some(RuntimeChoice::TokioMt),
            // PlainTest plus the "no dedicated rudzio runtime in v1"
            // family (async-std / actix / futures-test) all fall back
            // to --runtime default. These collapse to None — the
            // distinction is preserved upstream via
            // `needs_compat_warning`.
            Self::Actix | Self::AsyncStd | Self::FuturesTest | Self::PlainTest => None,
        }
    }

    /// Compatibility warning text for kinds whose runtime rudzio v1
    /// doesn't ship a dedicated equivalent for. Returns `None` when no
    /// warning is needed.
    #[inline]
    #[must_use]
    pub const fn needs_compat_warning(self) -> Option<&'static str> {
        match self {
            Self::Actix => Some(
                "actix runtime replaced with the --runtime default; actix-rt actor setup must be added manually if tests depend on it",
            ),
            Self::AsyncStd => Some(
                "async-std runtime replaced with the --runtime default; async-std-specific APIs may not behave identically",
            ),
            Self::FuturesTest => Some("futures-test runtime replaced with the --runtime default"),
            Self::Compio
            | Self::PlainTest
            | Self::TokioCurrent
            | Self::TokioDefault
            | Self::TokioMulti => None,
        }
    }
}

/// Outcome of classifying a single attribute: the kind, plus any extra
/// `tokio::test(...)` arguments the rewriter should warn about.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Detected {
    /// Tokio attribute arguments the rewriter cannot translate (e.g.
    /// `worker_threads`, `start_paused`) — preserved as a list of
    /// `key = value` strings for the warning channel.
    pub extra_tokio_args: Vec<String>,
    /// Classified kind of the attribute.
    pub kind: TestKind,
}

/// Returns the `T` in `#[test_context(T)]` if that's what this
/// attribute is; returns `None` otherwise.
#[inline]
#[must_use]
pub fn as_test_context(attr: &Attribute) -> Option<syn::Path> {
    if path_to_string(attr.path()) != "test_context" {
        return None;
    }
    let Meta::List(list) = &attr.meta else {
        return None;
    };
    list.parse_args::<syn::Path>().ok()
}

/// Classify an attribute. Returns `Some(_)` if it's a recognised
/// test/runtime macro; `None` otherwise (the rewriter then leaves the
/// attribute alone).
#[inline]
#[must_use]
pub fn classify_test_attr(attr: &Attribute) -> Option<Detected> {
    let path = path_to_string(attr.path());
    let kind = match path.as_str() {
        "test" => TestKind::PlainTest,
        "tokio::test" | "::tokio::test" => {
            return Some(classify_tokio_attr(attr));
        }
        "async_std::test" | "::async_std::test" => TestKind::AsyncStd,
        "compio::test" | "::compio::test" => TestKind::Compio,
        "actix_rt::test" | "::actix_rt::test" | "actix_web::test" | "::actix_web::test" => {
            TestKind::Actix
        }
        "futures_test::test" | "::futures_test::test" => TestKind::FuturesTest,
        _ => return None,
    };
    Some(Detected {
        kind,
        extra_tokio_args: Vec::new(),
    })
}

/// Tokio-specific helper: parse the attribute's nested-meta args to
/// pick `flavor = ...` and capture extras like `worker_threads` and
/// `start_paused` for the warning channel.
fn classify_tokio_attr(attr: &Attribute) -> Detected {
    let mut kind = TestKind::TokioDefault;
    let mut extras = Vec::new();

    if let Meta::List(list) = &attr.meta {
        let tokens = list.tokens.to_string();
        if tokens.contains("\"multi_thread\"") {
            kind = TestKind::TokioMulti;
        } else if tokens.contains("\"current_thread\"") {
            kind = TestKind::TokioCurrent;
        } else {
            // Neither flavor literal present — leave kind at default.
        }
        let _unused = attr.parse_nested_meta(|meta| {
            let name = meta.path.get_ident().map(ToString::to_string);
            match name.as_deref() {
                Some("flavor") => {
                    let _unused = meta
                        .value()
                        .and_then(syn::parse::ParseBuffer::parse::<syn::LitStr>);
                }
                Some("worker_threads") => {
                    if let Ok(parsed) = meta.value().and_then(syn::parse::ParseBuffer::parse::<Lit>)
                    {
                        extras.push(format!("worker_threads = {}", lit_to_string(&parsed)));
                    }
                }
                Some("start_paused") => {
                    if let Ok(parsed) = meta.value().and_then(syn::parse::ParseBuffer::parse::<Lit>)
                    {
                        extras.push(format!("start_paused = {}", lit_to_string(&parsed)));
                    }
                }
                _ => {
                    // Unknown arg — swallow without complaint.
                }
            }
            Ok(())
        });
    }

    Detected {
        kind,
        extra_tokio_args: extras,
    }
}

/// Returns `true` if the attribute is `#[bench]` (unstable libtest).
#[inline]
#[must_use]
pub fn is_bench_attr(attr: &Attribute) -> bool {
    path_to_string(attr.path()) == "bench"
}

/// Returns `true` if the attribute is any `#[ignore]` form, so the
/// rewriter knows to preserve it verbatim.
#[inline]
#[must_use]
pub fn is_ignore_attr(attr: &Attribute) -> bool {
    path_to_string(attr.path()) == "ignore"
}

/// Returns `true` if the attribute is from the `rstest` family.
///
/// That includes the outer `#[rstest]` wrapper plus the `#[case(...)]`
/// / `#[values(...)]` parameter-site markers. None of these have a
/// rudzio equivalent in v1; the conversion path should skip these fns
/// entirely.
#[inline]
#[must_use]
pub fn is_rstest_attr(attr: &Attribute) -> bool {
    let path = path_to_string(attr.path());
    matches!(
        path.as_str(),
        "rstest"
            | "::rstest::rstest"
            | "rstest::rstest"
            | "case"
            | "::rstest::case"
            | "rstest::case"
            | "values"
            | "::rstest::values"
            | "rstest::values"
    )
}

/// Returns `true` if the attribute is `#[should_panic]` /
/// `#[should_panic(expected = ...)]`.
#[inline]
#[must_use]
pub fn is_should_panic_attr(attr: &Attribute) -> bool {
    path_to_string(attr.path()) == "should_panic"
}

/// Render a `syn::Path` to its colon-separated string form.
#[inline]
#[must_use]
pub fn lit_to_string(lit: &Lit) -> String {
    match lit {
        Lit::Bool(boolean) => boolean.value.to_string(),
        Lit::Int(int) => int.base10_digits().to_owned(),
        Lit::Str(string) => format!("\"{}\"", string.value()),
        // Every non-Bool/Int/Str variant — and any future-added one,
        // since syn's `Lit` is `#[non_exhaustive]` — falls through to
        // the verbatim token-stream rendering.
        other @ (Lit::Byte(_)
        | Lit::ByteStr(_)
        | Lit::CStr(_)
        | Lit::Char(_)
        | Lit::Float(_)
        | Lit::Verbatim(_))
        | other => quote::ToTokens::to_token_stream(other).to_string(),
    }
}

/// Render a `syn::Path` to its colon-separated string form.
#[inline]
#[must_use]
pub fn path_to_string(path: &syn::Path) -> String {
    let mut out = String::new();
    if path.leading_colon.is_some() {
        out.push_str("::");
    }
    for (idx, seg) in path.segments.iter().enumerate() {
        if idx > 0 {
            out.push_str("::");
        }
        out.push_str(&seg.ident.to_string());
    }
    out
}
