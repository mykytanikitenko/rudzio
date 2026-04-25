//! Single `#[rudzio::main]` entry point for every test in the
//! `rudzio-macro-internals` crate. `linkme` collects every `#[rudzio::test]`
//! across the submodules below into one process.

mod args;
mod codegen;
mod test_attr_args;
mod transform;

#[rudzio::main]
fn main() {}
