set shell := ["bash", "-euo", "pipefail", "-c"]

# List available recipes
default:
    @just --list

# Enter nix development shell (just is aliased to use Justfile.nix inside)
nix:
    nix develop

# Format Rust code
fix-fmt:
    cargo fmt --all

# Check Rust formatting
check-fmt:
    cargo fmt --all --check

# Format TOML files
fix-taplo:
    taplo fmt **/Cargo.toml Cargo.toml

# Check TOML formatting
check-taplo:
    taplo fmt **/Cargo.toml Cargo.toml --check

# --- Linting ---

# Run cargo check
check:
    cargo check --workspace --all-features --all-targets

# Fix clippy warnings automatically
fix-clippy:
    cargo clippy --workspace --no-deps --all-features --all-targets --all --fix --allow-dirty --allow-staged -- -D warnings

# Check clippy lints
check-clippy:
    cargo clippy --workspace --no-deps --all-features --all-targets --all -- -D warnings

# Check for unused dependencies
check-udeps:
    cargo +nightly udeps --workspace --all-features --all-targets

# --- Testing ---

# Run all tests (loads .config/.env so integration tests can reach real APIs)
test:
    #!/usr/bin/env bash
    set -a && source .config/.env && set +a
    cargo test --workspace

# --- Aggregate ---

# Apply all automatic fixes
fix: fix-fmt fix-taplo fix-clippy

# Run all checks and tests (pre-commit)
pre-commit: check-fmt check-taplo check check-clippy check-udeps test
