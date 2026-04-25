//! Binary root for the `integration` test target. Pulls in
//! submodule files that hold the actual test fns.

mod models;

// Binary root itself has no tests — just the module wiring. This
// file is the one that gets `fn main` appended by the tool.

#[rudzio::main]
fn main() {}
