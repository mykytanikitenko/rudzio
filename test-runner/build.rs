//! Expose `rudzio-fixtures`'s fixture-bin executables as
//! `CARGO_BIN_EXE_<name>` env vars so the `#[path]`-included
//! `fixtures/tests/integration.rs` can resolve its `env!(...)` calls.

fn main() -> Result<(), rudzio::build::Error> {
    rudzio::build::expose_bins("rudzio-fixtures")
}
