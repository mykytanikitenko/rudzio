//! Library surface for the `cargo-rudzio` subcommand. Exposes the
//! aggregator-generation pipeline so integration tests can drive it
//! against synthetic inputs.

pub mod build_id;
pub mod cli;
pub mod config_env;
pub mod coverage;
pub mod generate;
