//! Pull rudzio's own dogfood suite (multi-runtime `Config` parser
//! tests) into this binary via `#[path]`. The file's macro-expanded
//! `#[rudzio::suite(...)]` emits its `linkme` statics here, so they
//! aggregate alongside macro-internals' statics into one `TEST_TOKENS`
//! slice.

#[path = "../../../tests/runner.rs"]
mod runner;
