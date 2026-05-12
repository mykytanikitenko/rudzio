//! Internals of `rudzio-macro`: AST-manipulation logic split out so that
//! integration tests (which cannot import items from a `proc-macro` crate)
//! can exercise it directly.
//!
//! Everything here operates on [`proc_macro2`] / `syn` types. The thin
//! `rudzio-macro` wrapper handles the `proc_macro::TokenStream` boundary
//! and delegates to these entry points.

pub mod codegen;
pub mod main_codegen;
pub mod parse;
pub mod proc_macro_env_codegen;
pub mod suite_codegen;
pub mod transform;
