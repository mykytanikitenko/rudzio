# In-nix Justfile — used when inside `nix develop`. Every recipe here
# depends on tools provided exclusively by the nix devShell (cargo,
# rustfmt, clippy, taplo, cargo-audit, cargo-deny, cargo-rudzio, etc.).
# The host-side Justfile (next to this one) is the entrypoint and
# `import`s this file; `just <recipe>` from inside a `nix develop`
# shell transparently picks up the recipes below.
#
# Why split host and nix:
#   * The recipes below 127 outside `nix develop` (cargo, taplo, etc.
#     are not on $PATH). Keeping them in a SEPARATE file makes the
#     split explicit at a glance.
#   * Anything that breaks outside nix lives here. Anything that works
#     unconditionally (gh, git, basic shell) lives in the host Justfile.
#   * The host Justfile was previously (pre-PR-#1) carrying an
#     `alias just='just -f Justfile.nix'` in flake.nix's shellHook;
#     that alias was removed by accident in 66fa12b. Using `import`
#     in the host Justfile (this PR) is the proper, idempotent
#     reincarnation of that wiring.
#
# `set shell := ...` lives ONLY in the host Justfile (settings are
# inherited through import; re-declaring here errors at parse time).

# --- Formatting ---

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
    cargo run -p cargo-rudzio -- test

# Per-crate stock path: `cargo test --workspace`. Useful when debugging a
# single crate's integration tests or reproducing what a user who doesn't
# have cargo-rudzio installed would see.
test-stock:
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
        --exclude rudzio-fixtures

# --- Demo ---

# Regenerate assets/demo.gif from assets/demo.sh.
#   * asciinema records the script to a .cast file (terminal-native, no
#     headless browser involved — works on minimal nix shells).
#   * agg (asciinema-agg) renders the cast as a gif.
# Requires asciinema and agg on PATH. The gif is committed so consumers
# don't need either tool to view the README.
demo:
    @command -v asciinema >/dev/null || { echo "missing: asciinema"; exit 1; }
    @command -v agg       >/dev/null || { echo "missing: agg (asciinema-agg)"; exit 1; }
    @# Make sure `cargo rudzio` resolves as a cargo subcommand during the
    @# recording — otherwise the demo would have to fall back to `cargo run`.
    cargo install --path cargo-rudzio --locked --quiet
    asciinema rec --quiet --overwrite \
        --cols 140 --rows 40 \
        --command 'bash assets/demo.sh' \
        /tmp/rudzio-demo.cast
    agg --speed 2.0 --theme monokai --cols 140 --rows 40 \
        /tmp/rudzio-demo.cast assets/demo.gif
    rm -f /tmp/rudzio-demo.cast
    @echo "→ assets/demo.gif regenerated ($(du -h assets/demo.gif | cut -f1))"

# --- Aggregates ---

# Dry-run `cargo publish` in dep order — sanity check before tagging.
# Pre-first-publish: each crate's dry-run will fail at the verify step
# because cargo strips path deps and tries to resolve workspace siblings
# from crates.io. Use `--no-verify` to package-only check, or run live
# in order via the release.yml workflow on a tag push.
release-dry-run:
    cargo publish --dry-run --no-verify -p rudzio-macro-internals
    cargo publish --dry-run --no-verify -p rudzio-macro
    cargo publish --dry-run --no-verify -p rudzio
    cargo publish --dry-run --no-verify -p rudzio-migrate
    cargo publish --dry-run --no-verify -p cargo-rudzio

# Apply all automatic fixes
fix: fix-fmt fix-taplo fix-clippy

# Run all checks and tests (pre-commit)
pre-commit: check-fmt check-taplo check check-clippy check-udeps test

# One command for the entire CI/CD pipeline — every gate the
# self-hosted runner executes on push to main runs here. `check-semver`
# is intentionally NOT in this aggregate until the first crates.io
# publish creates a baseline (it errors with "not found in registry"
# pre-publish). After v0.1.0 lands, add `check-semver` to this list.
ci: check-fmt check-taplo check-clippy check-audit check-deny test
