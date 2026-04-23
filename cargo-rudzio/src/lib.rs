//! Library surface for the `cargo-rudzio` subcommand. Exposes the
//! aggregator-generation pipeline so integration tests can drive it
//! against synthetic inputs.

#![allow(
    unused_results,
    clippy::needless_pass_by_value,
    reason = "toml_edit's insert/push API routinely returns the previous value; CLI glue does not care about the dropped option"
)]

pub mod generate;
