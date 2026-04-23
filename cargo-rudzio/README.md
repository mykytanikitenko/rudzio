# cargo-rudzio

Cargo subcommand for the [rudzio](https://github.com/mykytanikitenko/rudzio)
async test framework. Two jobs:

1. **`cargo rudzio test`** — runs every rudzio test in the workspace
   under one `#[rudzio::main]` binary, grouped by `(runtime, suite)`.
   One process, one scheduler, one summary. Tests that share a
   `(runtime, suite)` tuple collapse into one `Suite::setup` +
   `Suite::teardown` per group even when the declarations live in
   different files, different modules, or different crates. Filter
   args and all the rudzio config flags pass through to the runner.
2. **`cargo rudzio migrate`** — drives `rudzio-migrate`, the CLI that
   converts stock cargo `#[test]` / `#[tokio::test]` / `#[test_context(T)]`
   suites into rudzio shape.

## Install

```sh
cargo install cargo-rudzio
```

Or from source:

```sh
cargo install --path cargo-rudzio
```

## Commands

### `cargo rudzio test [ARGS...]`

On every invocation:

1. Reads `cargo metadata` for the current workspace.
2. Finds every member whose `Cargo.toml` deps include `rudzio`.
3. Generates an aggregator crate at
   `<workspace-target-dir>/rudzio-auto-runner/`:
   - `Cargo.toml` depends on rudzio (feature union across members) +
     every rudzio-using member + the union of their `[dev-dependencies]`
     so `use libc`, `use anyhow`, etc. resolve in the aggregated files.
   - `src/main.rs` is a two-line `#[rudzio::main] fn main() {}`.
   - `src/tests.rs` `#[path]`-includes every member's `tests/*.rs`
     (the shim `tests/main.rs` is skipped — it would redefine `main`).
   - `build.rs` shells out `cargo build --bins` for each bin-owning
     member into a sandboxed `CARGO_TARGET_DIR` and emits
     `cargo:rustc-env=CARGO_BIN_EXE_<name>` so integration tests that
     spawn sibling bins keep working.
4. Runs `cargo run --manifest-path <aggregator>/Cargo.toml -- ARGS`.
5. Propagates the aggregator's exit code.

Arg forwarding:

```sh
cargo rudzio test some_test_name_filter
cargo rudzio test --skip slow_test --threads 1
cargo rudzio test --bench               # switch bench-marked tests into full mode
cargo rudzio test --format json         # machine-readable output
```

Every `#[rudzio::test]` token across the workspace lands in one `linkme`
slice in the aggregator. The runner groups them by
`RuntimeGroupKey(runtime_path, suite_path)`, spawns one OS thread per
group, constructs the `Suite` once, runs every test in the group
against that `Suite`, tears down once. No test binary fragmentation.

### `cargo rudzio migrate [ARGS...]`

Thin dispatcher for `rudzio-migrate`. See
[`migrate/README.md`](../migrate/README.md) for the full flag reference.

```sh
cargo rudzio migrate --help
cargo rudzio migrate --path /path/to/your/crate
cargo rudzio migrate --path . --dry-run
cargo rudzio migrate --runtime tokio-ct --no-shared-runner
```

### `cargo rudzio generate-runner [--output DIR]`

Generates the aggregator without running it. Useful for inspecting
the generated manifest + sources, or for CI configurations that want
to build the aggregator once and run it many times. Default DIR is
`<target-dir>/rudzio-auto-runner`.

## Relationship to per-crate `cargo test`

`cargo rudzio test` and `cargo test -p <crate>` coexist:

- Stock `cargo test` still runs each crate's integration test binary
  (one per `[[test]]`), per cargo's execution model.
- `cargo rudzio test` produces ONE binary that links every member's
  tests together for cross-workspace runs.

Same test code, two run modes. Per-crate `cargo test` is what CI
matrix jobs want; `cargo rudzio test` is what workspace-wide runs
want when you care about shared resources, cross-crate grouping, or
a single summary line.

## Known limitations

- **Auto-regeneration cost.** Every `cargo rudzio test` regenerates
  the aggregator crate from scratch and recompiles it. The aggregator
  has its own target dir (`<target>/rudzio-auto-runner/target/`) so
  it doesn't share artifacts with the user's normal builds. Expect
  a 30s-1m first build per invocation; subsequent invocations
  incremental-compile quickly.

- **`trybuild` tests with manifest-dir-relative fixture paths don't
  work when `#[path]`-included**. The aggregator's
  `CARGO_MANIFEST_DIR` points at the aggregator, not the member. A
  future release will support `[package.metadata.rudzio] exclude` in
  member manifests to opt specific files out.

- **Feature unification across members is best-effort.** Today the
  aggregator enables rudzio features by scanning each member's
  direct deps on rudzio and taking the union. Rarely-used features
  may need explicit opt-in via CLI flag.

## License

Dual-licensed under MIT OR Apache-2.0, matching the rudzio project.
