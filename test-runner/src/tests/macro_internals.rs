//! Pull `rudzio-macro-internals` integration test files into this binary
//! via `#[path]`. The source files reference `rudzio_macro_internals::*`
//! (extern-crate form), which resolves fine here because this crate has
//! `rudzio-macro-internals` as a normal dep.

#[path = "../../../macro-internals/tests/args.rs"]
mod args;

#[path = "../../../macro-internals/tests/codegen.rs"]
mod codegen;

#[path = "../../../macro-internals/tests/transform.rs"]
mod transform;
