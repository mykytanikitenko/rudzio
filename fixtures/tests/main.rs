//! Single `#[rudzio::main]` entry point for the whole `rudzio-fixtures` test
//! binary. `linkme` gathers every `#[rudzio::test]` across the submodules
//! below, so both the integration scenarios and the trybuild compile checks
//! run from one process.

mod compile;
mod integration;

#[rudzio::main]
fn main() {}
