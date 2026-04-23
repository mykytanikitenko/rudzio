//! Test-attribute classification. Given a `syn::Attribute`, decide
//! whether it's a recognised test/runtime macro and, if so, what
//! runtime hint (if any) it carries.

use syn::{Attribute, Lit, Meta};

use crate::cli::RuntimeChoice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    /// Plain `#[test]`. No forced runtime.
    PlainTest,
    /// `#[tokio::test]` without `flavor=` → Tokio multi-thread.
    TokioDefault,
    /// `#[tokio::test(flavor = "multi_thread", ...)]`.
    TokioMulti,
    /// `#[tokio::test(flavor = "current_thread", ...)]`.
    TokioCurrent,
    /// `#[async_std::test]`.
    AsyncStd,
    /// `#[compio::test]`.
    Compio,
    /// `#[actix_rt::test]` / `#[actix_web::test]`.
    Actix,
    /// `#[futures_test::test]`.
    FuturesTest,
}

impl TestKind {
    pub const fn forced_runtime(self) -> Option<RuntimeChoice> {
        match self {
            Self::TokioMulti | Self::TokioDefault => Some(RuntimeChoice::TokioMt),
            Self::TokioCurrent => Some(RuntimeChoice::TokioCt),
            Self::Compio => Some(RuntimeChoice::Compio),
            Self::PlainTest => None,
            // async-std / actix / futures-test don't have dedicated rudzio
            // runtimes in v1. We rewrite them but emit a warning; the chosen
            // runtime falls back to --runtime default.
            Self::AsyncStd | Self::Actix | Self::FuturesTest => None,
        }
    }

    pub const fn needs_compat_warning(self) -> Option<&'static str> {
        match self {
            Self::AsyncStd => Some(
                "async-std runtime replaced with the --runtime default; async-std-specific APIs may not behave identically",
            ),
            Self::Actix => Some(
                "actix runtime replaced with the --runtime default; actix-rt actor setup must be added manually if tests depend on it",
            ),
            Self::FuturesTest => Some("futures-test runtime replaced with the --runtime default"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Detected {
    pub kind: TestKind,
    /// True if `#[tokio::test(worker_threads = ...)]` was present — the
    /// tool drops the arg silently and warns the user about it.
    pub extra_tokio_args: Vec<String>,
}

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

fn classify_tokio_attr(attr: &Attribute) -> Detected {
    let mut kind = TestKind::TokioDefault;
    let mut extras = Vec::new();

    if let Meta::List(list) = &attr.meta {
        let tokens = list.tokens.to_string();
        if tokens.contains("\"multi_thread\"") {
            kind = TestKind::TokioMulti;
        } else if tokens.contains("\"current_thread\"") {
            kind = TestKind::TokioCurrent;
        }
        let _unused = attr.parse_nested_meta(|meta| {
            let name = meta.path.get_ident().map(ToString::to_string);
            match name.as_deref() {
                Some("flavor") => {
                    let _unused = meta.value().and_then(|v| v.parse::<syn::LitStr>());
                }
                Some("worker_threads") => {
                    if let Ok(v) = meta.value().and_then(|v| v.parse::<Lit>()) {
                        extras.push(format!("worker_threads = {}", lit_to_string(&v)));
                    }
                }
                Some("start_paused") => {
                    if let Ok(v) = meta.value().and_then(|v| v.parse::<Lit>()) {
                        extras.push(format!("start_paused = {}", lit_to_string(&v)));
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

pub fn path_to_string(path: &syn::Path) -> String {
    let mut s = String::new();
    if path.leading_colon.is_some() {
        s.push_str("::");
    }
    for (i, seg) in path.segments.iter().enumerate() {
        if i > 0 {
            s.push_str("::");
        }
        s.push_str(&seg.ident.to_string());
    }
    s
}

pub fn lit_to_string(lit: &Lit) -> String {
    match lit {
        Lit::Str(s) => format!("\"{}\"", s.value()),
        Lit::Int(i) => i.base10_digits().to_owned(),
        Lit::Bool(b) => b.value.to_string(),
        other => quote::ToTokens::to_token_stream(other).to_string(),
    }
}

/// Returns `true` if the attribute is any `#[ignore]` form, so the
/// rewriter knows to preserve it verbatim.
pub fn is_ignore_attr(attr: &Attribute) -> bool {
    path_to_string(attr.path()) == "ignore"
}

/// Returns `true` if the attribute is `#[should_panic]` / `#[should_panic(expected = ...)]`.
pub fn is_should_panic_attr(attr: &Attribute) -> bool {
    path_to_string(attr.path()) == "should_panic"
}

/// Returns `true` if the attribute is `#[bench]` (unstable libtest).
pub fn is_bench_attr(attr: &Attribute) -> bool {
    path_to_string(attr.path()) == "bench"
}

/// Returns `true` if the attribute is from the `rstest` family — the
/// outer `#[rstest]` wrapper or the `#[case(...)]` / `#[values(...)]`
/// parameter-site markers. None of these have a rudzio equivalent in
/// v1; the conversion path should skip these fns entirely.
pub fn is_rstest_attr(attr: &Attribute) -> bool {
    let s = path_to_string(attr.path());
    matches!(
        s.as_str(),
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

/// Returns the `T` in `#[test_context(T)]` if that's what this attribute
/// is; returns `None` otherwise.
pub fn as_test_context(attr: &Attribute) -> Option<syn::Path> {
    if path_to_string(attr.path()) != "test_context" {
        return None;
    }
    let Meta::List(list) = &attr.meta else {
        return None;
    };
    list.parse_args::<syn::Path>().ok()
}
