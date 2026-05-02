# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-05-02

Initial public release. Five crates ship in lockstep:

- `rudzio` — async test framework with pluggable runtimes (tokio
  multi-thread / current-thread / local, compio, embassy, futures
  thread-pool) and per-test setup/teardown context.
- `rudzio-macro` / `rudzio-macro-internals` — `#[rudzio::main]`,
  `#[rudzio::test]`, and `rudzio::suite!` proc-macros.
- `cargo-rudzio` — `cargo rudzio test` aggregator that drives the
  `linkme`-based test registry across a workspace, plus
  `cargo rudzio migrate` for converting libtest tests in place.
- `rudzio-migrate` — standalone migrator for stock `cargo test`-style
  Rust tests; rewrites sources and Cargo.toml in place, keeps backups
  and pre-migration copies as block comments.

[0.1.0]: https://github.com/mykytanikitenko/rudzio/releases/tag/v0.1.0
