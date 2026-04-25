//! Multi-file test suite: tests defined across separate source files share a
//! single `rudzio::run()` entry point.
//!
//! Each included module has its own `#[rudzio::suite]` block. Because
//! `rudzio::TEST_TOKENS` is a `linkme::distributed_slice`, every module's
//! tokens are linked together at build time regardless of where they are
//! declared, and a single `#[rudzio::main]` picks them all up.

#[path = "multi_file_suite/module_a.rs"]
mod module_a;

#[path = "multi_file_suite/module_b.rs"]
mod module_b;

#[rudzio::main]
fn main() {}
