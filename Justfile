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

# Run the full aggregated rudzio suite via the auto-generated runner.
# Uses `cargo run -p cargo-rudzio -- test` so the recipe works on a fresh
# clone without requiring `cargo install cargo-rudzio`.
test:
    #!/usr/bin/env bash
    if [ -f .config/.env ]; then
        set -a && source .config/.env && set +a
    fi
    cargo run -p cargo-rudzio -- test

# Per-crate stock path: `cargo test --workspace`. Useful when debugging a
# single crate's integration tests or reproducing what a user who doesn't
# have cargo-rudzio installed would see.
test-stock:
    #!/usr/bin/env bash
    if [ -f .config/.env ]; then
        set -a && source .config/.env && set +a
    fi
    cargo test --workspace

# --- Security & policy ---

# Check for security advisories in the dep graph (RustSec)
check-audit:
    cargo audit

# Check license / advisory / source / banned-crate policy (deny.toml)
check-deny:
    cargo deny check

# Check API semver compatibility against the most recent crates.io release
check-semver:
    cargo semver-checks check-release --workspace \
        --exclude rudzio-migrate \
        --exclude rudzio-fixtures

# --- CI/CD ---

# Recent CI runs
ci-status:
    gh run list --workflow=ci.yml --limit 10

# Trigger CI on the current branch
ci-trigger:
    gh workflow run ci.yml --ref "$(git rev-parse --abbrev-ref HEAD)"

# Watch the most recent CI run
ci-watch:
    gh run watch

# Recent release runs
release-status:
    gh run list --workflow=release.yml --limit 10

# Watch the most recent release run
release-watch:
    gh run watch

# Dry-run `cargo publish` in dep order — sanity check before tagging
release-dry-run:
    cargo publish --dry-run -p rudzio-macro-internals
    cargo publish --dry-run -p rudzio-macro
    cargo publish --dry-run -p rudzio
    cargo publish --dry-run -p cargo-rudzio

# Tag current commit and instruct on push, e.g. `just release-tag 0.2.0`
release-tag VERSION:
    git tag -a "v{{VERSION}}" -m "Release v{{VERSION}}"
    @echo "Created tag v{{VERSION}}. Push it to trigger crates.io publish:"
    @echo "    git push origin v{{VERSION}}"

# --- Aggregate ---

# Apply all automatic fixes
fix: fix-fmt fix-taplo fix-clippy

# Run all checks and tests (pre-commit)
pre-commit: check-fmt check-taplo check check-clippy check-udeps test

# Mirror everything CI runs, locally
ci: check-fmt check-taplo check-clippy check-audit check-deny check-semver test
