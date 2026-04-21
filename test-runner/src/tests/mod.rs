//! Re-compiles every testable crate's integration-test source files
//! into this binary via `#[path]`. Each file registers its
//! `#[rudzio::test]` tokens through `linkme`, so the final `run()` sees
//! every test from every tracked crate in one slice.

mod fixtures;
mod macro_internals;
mod rudzio_self;
