//! Library surface for the `cargo-rudzio` subcommand. Exposes the
//! aggregator-generation pipeline so integration tests can drive it
//! against synthetic inputs.

pub mod generate;
pub mod sentinel;

pub use sentinel::{EXPOSE_BINS_SENTINEL_ENV, EXPOSE_BINS_SENTINEL_VALUE, spawn_env};
