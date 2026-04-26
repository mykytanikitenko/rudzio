# rudzio

Async test framework for Rust. Tests run against a three-layer lifecycle:
one `Runtime` per `(runtime, suite)` group, one `Suite` value per group
(shared across tests), one `Test` value per test (built in `Suite::context`
and torn down after the body). Runtimes are pluggable — tokio, compio,
embassy, futures-executor, or your own. Cancellation, per-test and per-run
timeouts, panic isolation, and SIGINT/SIGTERM are runner concerns, not
per-test concerns.

## Status

`0.1.x`. Not yet published to crates.io. The shape of `Suite`, `Test`,
`Runtime`, and the suite macro is intentionally stable — there are tests
asserting on rendered output format and on cancellation/teardown behaviour.
Internals (`SuiteRunner`, `TestToken` layout, `RuntimeGroupKey` hashing)
are `#[doc(hidden)]` and may change.

## Installation

Add rudzio as a git dependency. Pin to a commit for reproducibility.

```toml
[dev-dependencies]
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", features = ["common", "runtime-tokio-multi-thread"] }
```

Install the `cargo-rudzio` subcommand (provides both `cargo rudzio test`
and `cargo rudzio migrate`):

```sh
cargo install --git https://github.com/mykytanikitenko/rudzio cargo-rudzio
```

Or from a clone:

```sh
git clone https://github.com/mykytanikitenko/rudzio
cargo install --path rudzio/cargo-rudzio
```

The migrator can also be installed standalone if you don't want the
`cargo rudzio test` aggregator:

```sh
cargo install --git https://github.com/mykytanikitenko/rudzio rudzio-migrate
```

### Features

All off by default — pick what you need:

- `common` — ready-made `Suite`/`Test` pair on top of
  `CancellationToken` + `TaskTracker` at `rudzio::common::context`. Omit
  if you're writing your own context types.
- `runtime-tokio-multi-thread` — `rudzio::runtime::tokio::Multithread`
  (tokio's multi-thread runtime; pulls in `tokio/rt-multi-thread`).
- `runtime-tokio-current-thread` — `rudzio::runtime::tokio::CurrentThread`
  (tokio's current-thread runtime + `LocalSet`; `!Send` futures via a
  `SendWrapper` shim).
- `runtime-tokio-local` — `rudzio::runtime::tokio::Local` (tokio's
  `LocalRuntime`; native `!Send` support in `block_on`/`spawn_local`).
- `runtime-compio` — `rudzio::runtime::compio::Runtime`.
- `runtime-embassy` — `rudzio::runtime::embassy::Runtime`.
- `runtime-futures` — `rudzio::runtime::futures::ThreadPool` on top of
  `futures::executor::ThreadPool`. Dispatches each test onto an OS thread
  from the pool, which is the closest analogue to libtest's parallelism.
- `build` — `rudzio::build::expose_bins` / `expose_self_bins` helpers for
  build scripts (see "Spawning bins from tests" below).

The three `runtime-tokio-*` features are independent.

## When to use it

- Testing async code where `#[tokio::test]` isn't expressive enough —
  you want shared expensive setup (DB pool, container, server) created
  once per group, with per-test scopes underneath.
- The same test bodies need to run against multiple async runtimes
  without source duplication — e.g. verifying a library works under both
  tokio multi-thread and compio, or checking `!Send` behaviour under
  current-thread vs LocalRuntime.
- Graceful shutdown matters — root-token cancellation on SIGINT /
  SIGTERM / run timeout fans out into in-flight tests with teardown still
  running.

If you only need an async unit test, `#[tokio::test]` is fine. Reach for
rudzio when the lifecycle around the test matters as much as the test body.

## What to expect on adoption

- **Migration is mechanical when tests are isolated.** If each `#[test]`
  sets up its own fixtures and doesn't rely on side effects from previous
  tests, migration is a signature rewrite plus a `Cargo.toml` edit.
  `rudzio-migrate` handles both. Expected outcome on a well-isolated
  suite: compiles on first pass, tests behave the same.

- **Rudzio will surface global-state coupling between tests.** Libtest
  spawns one process per test binary and defaults to a thread-per-test
  model that masks cross-test state leaks (each test tends to see a cold
  process). Rudzio runs every test in a `(runtime, suite)` group inside
  ONE process against ONE shared `Suite` value. Tests that implicitly
  depended on process-start re-initialisation (static caches,
  `lazy_static`, `OnceCell` initialised at first call, env-var mutation
  without cleanup, global `tracing` subscribers registered on first use,
  unclosed database handles) will fail in ways they didn't before.
  The coupling was always there; rudzio makes it visible. If that
  diagnostic is unwelcome, pin the offending state inside `Suite::setup`
  or reset it in `Test::teardown`.

- **Synchronous tests still benefit.** Running sync bodies under
  `rudzio::runtime::futures::ThreadPool` dispatches each test onto an OS
  thread from the pool, matching libtest's parallelism model. Suite
  setup/teardown, cancellation, and the unified runner still apply. Use
  this regime if you want lifecycle hooks and cross-crate grouping
  without pulling in an async runtime.

## Quick example

```rust
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn first(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    async fn second(ctx: &Test) -> anyhow::Result<()> {
        ctx.yield_now().await;
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
```

`Cargo.toml`:

```toml
[[test]]
name = "integration"
path = "tests/main.rs"
harness = false  # rudzio replaces libtest

[dev-dependencies]
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", features = ["common", "runtime-tokio-multi-thread"] }
```

`harness = false` is required — the `#[rudzio::main]` attribute installs
the runner that walks every `#[rudzio::test]` registered via `linkme`.

### Return types

`#[rudzio::test]` bodies accept every libtest-compatible return shape
via the `rudzio::IntoRudzioResult` trait: bare `()`, explicit `-> ()`,
and `Result<T, E>` where `E: Display`. The runner converts each into
`Result<(), BoxError>` internally — `Ok` variants pass, `Err` is
recorded as the failure message.

```rust
// bare void: same shape as stock #[test]
#[rudzio::test]
fn assertion_only() {
    assert_eq!(1 + 1, 2);
}

// explicit unit: identical semantics
#[rudzio::test]
fn explicit_unit() -> () {}

// Result<(), E: Display>: failure path uses E's Display impl
#[rudzio::test]
async fn uses_anyhow() -> anyhow::Result<()> { Ok(()) }

#[rudzio::test]
async fn uses_std_io() -> Result<(), std::io::Error> { Ok(()) }

#[rudzio::test]
async fn uses_custom_enum() -> Result<(), MyError> { Ok(()) }
```

`rudzio-migrate` preserves whichever shape you had — no rewriting of
user signatures, no forced dependency on `anyhow`.

### The context parameter is optional

Zero-argument test bodies are first-class; the macro fills in the per-test
context at expansion time. Suite setup and per-test teardown still run.

```rust
#[rudzio::test]
async fn doesnt_need_the_context() -> anyhow::Result<()> {
    Ok(())
}
```

## Examples

Four runnable examples in `examples/`:

- `cargo run --example basic` — one runtime, the `common` context, a
  trivial suite (pass / yield / `#[ignore]`).
- `cargo run --example multi_runtime` — the same test bodies under
  tokio's Multithread + CurrentThread + compio, all in one
  `#[rudzio::suite]` block.
- `cargo run --example custom_context` — hand-rolled `Suite` / `Test`
  impls with shared suite-level state.
- `cargo run --example benchmark` (and `-- --bench`) — bench-annotated
  tests that run once as smoke tests by default and switch into full
  strategy execution under `--bench`.

## Concepts

Three traits, three lifetimes, strict outer-to-inner order:

```
'runtime  >  'suite_context  >  'test_context
```

| Trait                              | Lives for        | Created                           | Dropped                          |
|------------------------------------|------------------|-----------------------------------|----------------------------------|
| `Runtime<'rt>`                     | `'runtime`       | once per `(runtime, suite)` group | when the group thread exits     |
| `Suite<'suite_context, R>`         | `'suite_context` | once per group, after `Runtime`   | after the last test in the group |
| `Test<'test_context, R>`           | `'test_context`  | once per test, in `Suite::context`| after the test body returns     |

`Self::Test` on `Suite` is a GAT — `Self::Test<'test_context>` — so the
per-test context value genuinely lives in the per-test borrow lifetime,
not in the suite's. That's what makes `&mut TestCtx` parameters compile.

`#[rudzio::test]` accepts `&Ctx`, `&mut Ctx`, or no parameter at all as
the first argument, sync or async body, returning any
`Result<T, E: Display>`. Zero-arg test bodies still see full per-test
setup + teardown — they just don't receive the context.

## How rudzio runs: three regimes

Rudzio runs tests in three modes, chosen by invocation. They coexist on
the same source tree. Pick per task.

### 1. Stock `cargo test` (libtest)

The default for an unmigrated crate. `#[test]`, `#[tokio::test]`,
`#[test_context]` go through libtest's dispatcher. Rudzio is not involved.
Listed here because rudzio-migrated crates still leave doctests on this
path — `cargo test --doc` always invokes libtest.

### 2. `cargo test` over rudzio (per-crate `#[rudzio::main]`)

After migration, each crate has one test binary whose entry point is
`#[rudzio::main]` with `harness = false`. `cargo test -p <crate>` builds
and runs that binary. Every `#[rudzio::test]` compiled into it is
collected by `linkme`, grouped by `(runtime, suite)` tuple, scheduled,
summarised. Stock cargo invocation, rudzio execution.

Shape:

```toml
# <crate>/Cargo.toml
[lib]
harness = false            # if src/** has #[rudzio::suite] mods

[[test]]
name = "main"
path = "tests/main.rs"
harness = false            # for tests/*.rs integration suites
```

What you keep: `cargo test`, `cargo test -p <crate>`,
`cargo test --workspace`, IDE test runners, CI matrix jobs that shard by
crate. Each crate's test binary is independent; no cross-crate grouping.

What you gain over regime 1: suite lifecycle (setup/teardown),
cancellation, multi-runtime dispatch, per-test timeouts, panic isolation,
structured benchmarking.

### 3. `cargo rudzio test` (workspace aggregator)

One binary, one `#[rudzio::main]`, every rudzio test in the workspace.
`cargo-rudzio` reads `cargo metadata`, generates an aggregator crate at
`<target>/rudzio-auto-runner/`, builds it, runs it. Same test sources —
no source-level changes vs regime 2.

Consequences, all derived from "one process":

- `(runtime, suite)` tuples dedupe across crates: four crates sharing
  `(tokio::Multithread, common::Suite)` produce one `Suite::setup`, one
  `Suite::teardown`, one OS thread, one shared suite value.
- Rudzio-level config (`--threads`, `--test-timeout`, `--skip`, filter
  patterns, `--bench`, `--format`) applies uniformly across all workspace
  tests; one summary.
- Test output belongs to one terminal region with live per-runtime
  drawers, rather than N interleaved libtest streams.
- `src/**` unit tests behind `#[cfg(any(test, rudzio_test))]` gates fire
  under this regime too. The aggregator's generated `build.rs` emits
  `cargo:rustc-cfg=rudzio_test` for the aggregator compile unit, and
  each bridge's generated `build.rs` does the same for the bridge unit
  — so the cfg scopes exactly to the crates that need it, without
  leaking into nested cargo invocations. Under regime 2, the
  `cfg(test)` arm fires instead.

To make dev-deps visible when the aggregator compiles a member as a
plain lib, `cargo-rudzio` generates per-member "bridge" crates at
`<target>/rudzio-auto-runner/members/<name>/Cargo.toml`. Each bridge
re-points `[lib] path` at the real `src/lib.rs` but owns its own
`[dependencies]` table (the merged member `[dependencies]` +
`[dev-dependencies]`). Member `Cargo.toml` stays pristine — no
machinery leaks into user manifests.

### Choosing

| Situation | Regime |
|---|---|
| Unmigrated crate, running doctests, wanting the libtest baseline | 1 |
| Single-crate dev loop, CI matrix sharded by crate, IDE test navigation | 2 |
| Workspace-wide runs with shared suite state across crates, single summary, one scheduler | 3 |

Regimes 2 and 3 produce the same pass/fail set on the same test bodies
(modulo scheduler order). Regime 3 does more, in one binary.

## `cargo rudzio test` — using the aggregator

```sh
cargo rudzio test                         # build + run the aggregator
cargo rudzio test some_filter             # forward args to the runner
cargo rudzio test --skip slow --threads 1 --bench
```

On every invocation, cargo-rudzio:

1. Reads `cargo metadata` for the current workspace.
2. Finds every member whose `Cargo.toml` deps include `rudzio`.
3. Generates an aggregator crate at
   `<workspace-target-dir>/rudzio-auto-runner/`:
   - `Cargo.toml` depends on rudzio (feature union across members) + every
     rudzio-using member + the union of their `[dev-dependencies]`.
   - `src/main.rs` is a two-line `#[rudzio::main] fn main() {}`.
   - `src/tests.rs` `#[path]`-includes every member's `tests/*.rs` under
     `mod tests { mod <crate> { mod <file>; … } }` — per-crate namespaces
     mean sibling test files can share helpers via `use super::helper::*`.
   - `build.rs` shells out `cargo build --bins` for each bin-owning member
     into a sandboxed `CARGO_TARGET_DIR` and emits
     `cargo:rustc-env=CARGO_BIN_EXE_<name>` so tests that spawn sibling
     bins keep working.
4. Runs `cargo run --manifest-path <aggregator>/Cargo.toml -- ARGS`.
5. Propagates the aggregator's exit code.

Every `#[rudzio::test]` across every member lands in one `linkme` slice;
the runner groups tokens by `RuntimeGroupKey` (FNV of
`runtime_path :: suite_path`), spawns one OS thread per group, constructs
the `Suite` once, runs every test in that group against that `Suite`,
tears down once — across crate boundaries, not per file.

Also exposed:

- `cargo rudzio migrate [ARGS...]` — drives `rudzio-migrate` (same flags
  as the standalone binary).
- `cargo rudzio generate-runner [--output DIR]` — regenerates the
  aggregator without running it; useful for inspection or for committing
  as the starting point of a hand-rolled runner.

ARGS after the subcommand name are forwarded to the aggregator binary.
It accepts rudzio's full config flag set (filter patterns, `--skip`,
`--bench`, `--format`, `--threads`, `--test-timeout`, `--run-timeout`).

### How bridges expose dev-deps (default)

When the aggregator pulls a member in as a plain lib `[dependencies]`
entry, cargo does NOT activate the member's `[dev-dependencies]`.
`use ::rudzio::...` inside a `src/**` test module would therefore fail
to resolve.

`cargo-rudzio` handles this transparently: for every member with
`src/**` rudzio suites, it generates a per-member bridge crate at
`<target>/rudzio-auto-runner/members/<name>/Cargo.toml`. The bridge
declares `[lib] path = "<real>/src/lib.rs"` (so cargo compiles the
member's source tree under the bridge's identity) plus a
`[dependencies]` table that merges the member's `[dependencies]` +
`[dev-dependencies]`. The aggregator's own Cargo.toml then references
the bridge via `<member> = { path = "./members/<name>", package =
"<name>_rudzio_bridge" }`, so `extern crate <member>;` in the
aggregator resolves to the bridge's rlib.

Consequence: the member's own `Cargo.toml` stays pristine — no
`[target."cfg(rudzio_test)"...]` block, no rudzio-specific keys.

Caveat: bridges can't paper over dev-dep cycles. If crate A has B in
`[dev-dependencies]` and B has A in `[dependencies]`, cargo rejects the
cycle (it tolerates cycles only in `[dev-dependencies]`, not in
`[dependencies]`). Rearrange your deps or move the affected test into
`tests/*.rs` (integration tests compile into the aggregator's own unit
and bypass the bridge).

### Inspecting the generated aggregator

The auto-generator writes the aggregator crate to
`<target-dir>/rudzio-auto-runner/` and leaves it there after the run. If
a test doesn't show up, open that directory and read the generated
`Cargo.toml`, `src/main.rs`, `src/tests.rs`, and `build.rs` — plain Rust +
TOML with no template-language fluff.

### Excluding specific test files from aggregation

Some `tests/*.rs` files have manifest-dir-relative paths that only
resolve when compiled as part of their owning crate — the classic case is
a `trybuild::TestCases` harness pointing at `tests/fixtures/<name>.rs`.
Those files should run under stock `cargo test -p <crate>` but be skipped
by the aggregator.

Add `[package.metadata.rudzio].exclude` to the member's `Cargo.toml`:

```toml
[package.metadata.rudzio]
exclude = ["tests/compile.rs"]
```

Paths are resolved relative to the member's manifest directory.

### Feature unification

Cargo unifies features across every workspace member's deps. The
generator's aggregator sits in `<target-dir>/rudzio-auto-runner/` with
its own empty `[workspace]` stanza, so its feature selection does not
leak back into the parent workspace. `cargo rudzio test` does not alter
the feature set that `cargo build --workspace` sees.

### Hand-rolled aggregator

If you need customisation the auto-generator doesn't do — different
feature selections per `(runtime, suite)` group, source-level
preprocessing of included test files, a committed aggregator so CI can
build it without `cargo-rudzio` on the PATH — generate the shape once,
commit it, then edit freely:

```sh
cargo rudzio generate-runner --output my-runner
```

The generated output is plain Rust + TOML. Same structure as the
auto-generated aggregator; the only difference is that it's checked in.

## Multiple runtimes per test

Each tuple in the `#[rudzio::suite]` list is a separate `(runtime, suite)`
configuration. The same test bodies run against each:

```rust
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    (
        runtime = rudzio::runtime::compio::Runtime::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #[rudzio::test]
    async fn runs_on_every_runtime(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}
```

The runner spawns one OS thread per `(runtime, suite)` pair. Multiple
`#[rudzio::suite]` blocks declaring the same `(runtime, suite)` collapse
into one thread / one runtime / one suite instance — keyed by a
compile-time hash of the `(runtime_path, suite_path)` token strings.

## Runtimes

Behind feature flags. Default: none.

- `runtime-tokio-multi-thread` → `rudzio::runtime::tokio::Multithread`
- `runtime-tokio-current-thread` → `rudzio::runtime::tokio::CurrentThread`
- `runtime-tokio-local` → `rudzio::runtime::tokio::Local`
- `runtime-compio` → `rudzio::runtime::compio::Runtime`
- `runtime-embassy` → `rudzio::runtime::embassy::Runtime`
- `runtime-futures` → `rudzio::runtime::futures::ThreadPool`

Implementing your own `Runtime<'rt>` is a regular trait impl; the runner
is not hard-coded to any runtime crate.

## Custom contexts

`rudzio::common::context` ships a ready-to-use `(Suite, Test)` pair on top
of `tokio_util::sync::CancellationToken` + `tokio_util::task::TaskTracker`
(enable the `common` feature). For your own (a `sqlx::PgPool`, an HTTP
server handle, a mock clock), define structs that implement
`rudzio::context::Suite` and `rudzio::context::Test`. See
`fixtures/src/bin/custom_context_tokio_mt.rs` for a minimal hand-rolled
example.

## Lib unit tests (no `tests/` directory)

If your tests live inside `src/` under `#[cfg(test)] mod tests { ... }`
blocks — the classic rust-lang unit-test shape — you can run them through
rudzio without adding a `tests/` directory. Two edits:

```toml
# Cargo.toml
[lib]
harness = false            # opt out of libtest for the lib's own test target

[dev-dependencies]
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", features = ["runtime-tokio-multi-thread", "common"] }
```

```rust
// src/lib.rs
pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::*;

    #[rudzio::test]
    async fn sums_correctly() -> anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        Ok(())
    }
}

// Entry point for the lib's test target (harness = false). cfg(test)
// keeps it out of downstream binaries that depend on this lib.
#[cfg(test)]
#[rudzio::main]
fn main() {}
```

`cargo test --lib` now runs through `#[rudzio::main]`. `rudzio-migrate`
wires both edits automatically when it converts a crate with `src/` unit
tests.

## Same-crate single-binary test runner (unit + integration + e2e)

If every flavour of test lives in one crate — unit tests inside `src/`,
integration tests in `tests/integration/`, e2e tests behind a feature
flag — scheduled by one `#[rudzio::main]`, you can do it without a
separate aggregator crate:

```toml
# Cargo.toml
[lib]
harness = false

[features]
integration = []
e2e         = []

[dev-dependencies]
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", features = ["runtime-tokio-multi-thread", "common"] }
```

```rust
// src/lib.rs
#[cfg(test)]
extern crate self as my_crate;         // crate:: paths in the includes resolve to this binary

// Unit tests inside `src/` register normally via their own
// `#[cfg(test)] #[rudzio::suite(...)] mod tests { ... }` blocks.

// Integration tests: pull tests/integration/mod.rs into the lib's test
// target so their linkme entries register in the same slice.
#[cfg(all(test, feature = "integration"))]
#[path = "../tests/integration/mod.rs"]
mod integration;

#[cfg(all(test, feature = "e2e"))]
#[path = "../tests/e2e/mod.rs"]
mod e2e;

#[cfg(test)]
#[rudzio::main]
fn main() {}
```

`cargo test --lib` runs unit tests. `cargo test --lib --features
integration` adds integration. `cargo test --lib --features
integration,e2e` adds e2e. All three share the same `#[rudzio::main]`
binary and the same scheduler pass.

## Spawning bins from tests: `rudzio::bin!`

`rudzio::bin!("<name>")` returns a `PathBuf` to the named bin. Same call
site works across every rudzio layout — `tests/*.rs` integration tests,
shared aggregators, `cargo test --lib` runners:

```rust
let mut child = std::process::Command::new(rudzio::bin!("my-server"))
    .arg("--port=0")
    .spawn()?;
```

Resolution chain:

1. `option_env!(concat!("CARGO_BIN_EXE_", <name>))` at compile time.
   Cargo populates this automatically for `tests/*.rs` integration tests
   of the crate that declares the `[[bin]]`; rudzio's `expose_bins` /
   `expose_self_bins` build-script helpers populate it for shared-
   aggregator and `cargo test --lib` layouts.
2. Runtime walk from `std::env::current_exe()` to
   `target/<profile>/<name>` if step 1 missed. Covers the
   `cargo test --lib` layout when the user pre-builds with
   `cargo build --bins` instead of adding a build script.
3. Panic naming the bin and pointing at the two fixes (pre-build, or add
   `build.rs` with `expose_self_bins`) if both miss.

### Aggregating tests that spawn `[[bin]]` targets

Tests `#[path]`-included into an aggregator crate lose access to
`CARGO_BIN_EXE_<name>`: cargo only populates those for integration tests
of the crate that declares the `[[bin]]`.

Rudzio ships `rudzio::build::expose_bins` (behind the `build` feature) for
this. The auto-generated aggregator's `build.rs` calls the equivalent
for you; if you hand-roll, call it once from your aggregator's `build.rs`:

```toml
# my-runner/Cargo.toml
[dependencies]
my-bin-crate = { path = "../my-bin-crate" }

[build-dependencies]
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", default-features = false, features = ["build"] }
```

```rust
// my-runner/build.rs
fn main() -> Result<(), rudzio::build::Error> {
    rudzio::build::expose_bins("my-bin-crate")
}
```

The build script reads `cargo metadata` for `my-bin-crate`, runs
`cargo build --bins -p my-bin-crate` with a dedicated target dir
(`$OUT_DIR/rudzio-bin-cache`, so there's no lock contention with the outer
cargo), and emits `cargo:rustc-env=CARGO_BIN_EXE_<name>=<abs path>` for
each bin. `rudzio::bin!("<name>")` call sites in the `#[path]`-included
tests resolve via step 1 of the chain.

For the "this crate's bins for its own `cargo test --lib` runner" case,
use `rudzio::build::expose_self_bins()` — it reads `CARGO_PKG_NAME` and
delegates to `expose_bins`.

A re-entry sentinel (`__RUDZIO_EXPOSE_BINS_ACTIVE`, set on every nested
cargo) breaks the recursion that would otherwise happen when the nested
`cargo build --bins -p <self>` re-runs the same build script.

Dep requirement: the bin crate must have a lib target (even empty
`src/lib.rs`) so cargo accepts it as a regular dep. Missing env var,
missing package in metadata, missing bin, nested build failure → build
script errors with an explicit message.

## Borrowing from the `Suite` (HRTB asymmetry)

`Suite<'suite_context, R>` requires `R: for<'r> Runtime<'r>` (the runtime
is reused across every per-test borrow), while `Test<'test_context, R>`
requires only `R: Runtime<'test_context>` (one specific lifetime). The
asymmetry is deliberate — it lets a test hold a borrow of the suite
without dragging the HRTB bound onto the test type — but it has a sharp
edge:

```rust
// Hold `&Suite` in Test → HRTB requirement bubbles up onto Test's R,
// and the `#[rudzio::test]` macro's call site resolves R under the
// *non*-HRTB bound → trait bound for<'__r> R: rudzio::Runtime<'__r>
// is not satisfied at the test fn.
pub struct MyTest<'test_context, R> {
    pub suite: &'test_context MySuite<'test_context, R>,   // ✗ propagates HRTB
}
```

Workaround: have the `Test` borrow *specific fields* of the suite, not
`&Suite` itself. Nothing in the rudzio API asks for `&Suite` —
`Suite::context` decides what the per-test value contains:

```rust
pub struct MyTest<'test_context> {
    pub pool: &'test_context sqlx::PgPool,   // ✓ bare borrow, no R propagation
}
```

When the test only needs a couple of suite-owned handles, threading those
through avoids the HRTB mismatch and keeps the generated macro call site
simple.

## Concurrency knobs

Rudzio has **three** knobs that control how many things happen at once.
They compose — each one caps a different layer of the pipeline — and
understanding all three is worth a minute of reading.

| Flag | Layer it caps | Default |
|---|---|---|
| `--test-threads=<N>` | Each runtime's internal worker pool (tokio multi-thread workers, compio reactors, …) | [`std::thread::available_parallelism`] |
| `--concurrency-limit=<N>` | In-flight futures per `(runtime, suite)` group — the scheduler's `FuturesUnordered` ceiling | `--test-threads` |
| `--threads-parallel-hardlimit=<value>` | Total test bodies actively polling across the **whole run**, summed over every group and every runtime | `--test-threads` |

**Why three?** Because the workspace aggregator (`cargo rudzio test`)
runs every `(runtime, suite)` group as a separate OS thread, each with
its own runtime worker pool. If you have 4 groups and
`--concurrency-limit=4` per group, you can have up to 16 test bodies
polling in parallel. On an 8-core CI runner that thrashes badly.
`--threads-parallel-hardlimit` is the outermost cap that enforces "no
more than `<N>` tests executing across the whole process, full stop".

```
--test-threads=4  --concurrency-limit=4  --threads-parallel-hardlimit=4
             ^ per-runtime worker pool     ^ total across the run
                            ^ per-group in-flight ceiling
```

### `--threads-parallel-hardlimit` values

- *(flag omitted)* — gate is on at the current `--test-threads` value.
- `=<N>` — explicit numeric cap.
- `=threads` — explicit spelling of the default (`--test-threads` value).
- `=none` — gate disabled (previous unbounded behaviour).

When a test future tries to start but the gate is saturated, the OS
thread polling it **really parks** on a `std::sync::Condvar` (not a
cooperative async semaphore). This is intentional: it's what "hard
limit" means and it's what keeps the aggregator from overcommitting
cores. When a thread unparks, a single line is written to the test's
stdout (so the runner attributes it to the right test block):

```
rudzio: parked 1.3ms on parallel-hardlimit (8 max); disable with --threads-parallel-hardlimit=none
```

If you see these lines regularly and don't want the cap, pass
`--threads-parallel-hardlimit=none`. If you see them occasionally and
the wall-clock is fine, ignore them — the gate did its job.

### Interaction with `--bench`

Benchmark timing is sensitive to Condvar wake-ups, so `--bench` **auto-
disables the gate** as long as you didn't pass `--threads-parallel-
hardlimit` yourself. An explicit value (including `=none`) always wins.

### Current-thread runtime caveat

On single-threaded runtimes (`tokio::CurrentThread`,
`futures::LocalPool`, `embassy`), setting `--threads-parallel-hardlimit`
lower than that runtime's `--concurrency-limit` can deadlock: with `N`
permits held on the single thread, the (N+1)th future on the same
thread parks on the Condvar and blocks every other future from making
progress, including the ones that would release the permits. The
honest implementation exposes this rather than working around it — set
the hardlimit at least as high as the largest current-thread
`--concurrency-limit` in your run.

## Benchmarks

Any `#[rudzio::test]` can also run as a benchmark by adding a
`benchmark = <strategy>` argument to the attribute. Without `--bench`,
the body runs exactly once as a regular test (smoke mode is the default
— the `benchmark = ...` expression isn't even evaluated). With `--bench`,
the runner dispatches through the strategy, collects per-iteration
timings, and prints a distribution.

```rust
#[rudzio::test(benchmark = rudzio::bench::strategy::Sequential(1000))]
async fn query_latency(ctx: &Test) -> anyhow::Result<()> {
    ctx.yield_now().await;
    Ok(())
}

#[rudzio::test(benchmark = rudzio::bench::strategy::Concurrent(100))]
async fn under_load(_ctx: &Test) -> anyhow::Result<()> {
    Ok(())
}
```

Stock strategies (`rudzio::bench::strategy`):

- `Sequential(N)` — run the body `N` times one after another.
- `Concurrent(N)` — drive `N` copies of the body concurrently on the
  same task via `futures::join_all` (no spawn, works under `!Send`
  runtimes like compio / embassy / tokio `Local`).

The `Strategy` trait is one method; add your own (warm-up then measure,
repeat-K-rounds, rate-limit-to-X-rps) by writing a new impl. The
attribute's argument is a Rust expression — the macro evaluates whatever
value you give it at the call site, no registry.

`--bench`-mode output includes a one-line `[BENCH]` status with headline
figures, a detailed multi-line statistics block (sample count, wall-clock,
throughput, min/max/range, mean, median, σ, MAD, coefficient of
variation, IQR, outlier count, percentiles from p1 through p99.9), and an
ASCII histogram. Bench-annotated tests with `&mut Ctx` are rejected at
macro time because the strategy calls the body repeatedly (Concurrent
would clash on the exclusive borrow).

CLI flags:

- *(default)* — smoke mode: body runs once, regardless of
  `benchmark = ...`.
- `--bench` — full mode: run every bench-annotated test through its
  strategy. Non-bench tests still run.
- `--no-bench` — skip mode: bench-annotated tests are reported as
  ignored (useful on slow CI).

`--bench` also auto-disables `--threads-parallel-hardlimit` (unless you
set it explicitly) so Condvar wake-ups don't muddy the timing — see the
Concurrency knobs section above.

### Precision expectations

Rudzio's benchmark facility is built for rational estimation, not
nanosecond-grade precision. Per-iteration measurement wraps the body in
`AssertUnwindSafe + catch_unwind`, reads `Instant::now()` before and
after, and pushes a `Duration` into a `Vec` — a pipeline whose own cost
sits in the tens of nanoseconds, before the async runtime's polling and
waker overhead. The strategies do none of the things a serious
nanosecond harness would do: no adaptive iteration budget, no warm-up
passes, no linear regression against iteration count, no Tukey outlier
rejection, no CPU pinning, no `std::hint::black_box`.

The numbers are useful when the signal sits comfortably above the
measurement floor:

- **Regression tracking.** Yesterday's `Sequential(1000)` p50 was 45 µs;
  today it's 70 µs — real signal.
- **Distribution-shape comparison.** Run the same body under
  `Sequential(N)` and `Concurrent(N)` and compare how p99 and the
  coefficient of variation diverge.
- **Integration-level work.** End-to-end flows involving IO, allocation,
  or cross-thread coordination, where per-iteration cost is orders of
  magnitude above the instrument's own overhead.

For single-nanosecond precision — micro-benchmarks of hot inner loops,
cache-sensitive numeric kernels — use [criterion][criterion] or a direct
perf-counter harness.

[criterion]: https://crates.io/crates/criterion

## Cancellation, timeouts, panics

- **`--test-timeout=N`** (seconds): per-test budget. On expiry the
  per-test cancellation token is cancelled (test body sees the signal)
  and teardown still runs.
- **`--run-timeout=N`** (seconds): whole-run budget. Cancels the root
  token; in-flight tests cooperatively wind down, queued tests are
  reported as cancelled, teardowns run.
- **SIGINT / SIGTERM**: same as run timeout. No `kill -9`-shaped exits
  as long as your tests respect their cancellation token.
- **Panics**: each test body and every teardown is wrapped in
  `catch_unwind`. A panicking test is counted as `FAILED (panicked)`; a
  panicking teardown is logged as a warning but doesn't take down the run.

Each suite group gets a child of the run-wide root token, so a
`Suite::teardown` that cancels its stored token only fans out within its
own group.

## Output capture

Each test's stdout/stderr is captured and printed attributed to that test.
On Unix, rudzio does FD-level `dup`/`dup2` of fds 1 and 2, installs
anonymous pipes in their place, widens the kernel buffer via `F_SETPIPE_SZ`,
and restores the originals on guard drop. Output is buffered per-test and
emitted on completion; live drawer mode shows in-flight tests separately.
On Windows, rudzio falls back to libtest's `set_output_capture` thread-local
shim.

## Doctests stay on libtest

`cargo test --doc` is not intercepted by rudzio. Doctests continue to run
through libtest's dynamic dispatcher. If you want doctest bodies under
rudzio's lifecycle, move them into `#[rudzio::test]` fns.

## `#[cfg(any(test, rudzio_test))]`

`cargo test` activates `cfg(test)`. `cargo rudzio test` activates
`rudzio_test` per compile unit — the aggregator's generated `build.rs`
emits `cargo:rustc-cfg=rudzio_test`, and each bridge's generated
`build.rs` does the same. This does NOT activate `cfg(test)` — the
aggregator is not the per-crate test target, so from cargo's
perspective the member is built as a regular lib. Per-compile-unit
emission (rather than ambient RUSTFLAGS) means the cfg scopes exactly
to the aggregator and bridges; nested cargo invocations (e.g. the
`cargo build --bins` spawned for bin-member exposure) inherit none of
it.

`rudzio-migrate` rewrites `#[cfg(test)] mod tests` →
`#[cfg(any(test, rudzio_test))] mod tests` on modules carrying rudzio
suites, so the same module compiles under both regimes. Same for
`#[cfg_attr(test, ...)]` (conditional derives etc.): migrator broadens to
`#[cfg_attr(any(test, rudzio_test), ...)]`.

Without this broadening, `cargo rudzio test` sees the suite module as
cfg-gated-out and the tests silently vanish. The migrator handles it
automatically; hand-written rudzio suites in `src/**` should use the
broader cfg gate.

## Thread-safety requirements

Rudzio shares one `Suite` value across all tests in a group. `Suite` must
be `Send + Sync`. `Suite::Test<'_>` is free to be `!Send` if the runtime
is single-threaded (current-thread, `LocalRuntime`, compio, embassy) —
the runner knows not to move test contexts across tasks for these.
Multi-threaded runtimes (`Multithread`, `futures::ThreadPool`) require
`Send` contexts.

## IDE integration

Regime 2 (per-crate `cargo test`) works with rust-analyzer's test runner
out of the box — gutter icons, `Run Test` commands, per-test debug
launches all invoke `cargo test` which dispatches into `#[rudzio::main]`.

Regime 3 (`cargo rudzio test`) is CLI-only. rust-analyzer doesn't know
about the aggregator; gutter icons will not run tests via it. Use regime 2
for interactive editing, regime 3 for batch runs.

## CI recipes

- **Matrix sharding by crate** → regime 2. Each job runs
  `cargo test -p <crate>`; parallelism at the CI level. Failure
  attribution per-crate is trivial. Good for PR-gating runs.
- **Consolidated workspace run** → regime 3. One job runs
  `cargo rudzio test`; one summary, cross-crate suite dedup. Good for
  nightly / post-merge.
- **Both.** Nothing about the regimes conflicts — both can run in the
  same pipeline.

## Compile-time and binary-size cost

`linkme`-registered tests add a small constant per test (one static per
`#[rudzio::test]`). `#[rudzio::suite]` expansion generates one
suite-group-owner static per tuple. Neither scales poorly.

The aggregator's first build recompiles the rudzio-using dependency
graph; expect 30s–60s on a medium workspace. Subsequent `cargo rudzio
test` runs incremental-compile in seconds. The aggregator has its own
target dir (`<target>/rudzio-auto-runner/target/`) so it doesn't share
artifacts with your normal builds — on disk this is a 1–2 GB cache
depending on the workspace's dep set.

## What rudzio does NOT do

- **No `#[should_panic]` equivalent.** Rudzio has no panic-expectation
  attribute. Rewrite the body to catch the panic explicitly if you need it.
- **No parameterised tests.** No `#[rstest]`-shaped matrix expansion over
  the body. The `#[rudzio::suite([...])]` tuple does per-runtime matrix
  but not per-test-parameter.
- **No fuzzing / property-based integration.** `proptest` and `quickcheck`
  work inside test bodies the same as under libtest; rudzio doesn't wrap
  them.
- **No mocking.** Use `mockall`, `mockito`, `wiremock`, etc. as you would
  elsewhere.
- **No nanosecond-grade benchmark facility.** See "Precision expectations"
  above; reach for `criterion` when you need it.
- **No libtest JSON format.** `--format=pretty|terse` cover the built-in
  options. Custom runtimes can parse `Config::unparsed` to add others.

## Recommended lint configuration

If your crate runs with strict lints, a few defaults need relaxing for
rudzio's macros and test binaries:

```rust
// src/lib.rs (your crate)
#![deny(unsafe_code)]                   // not `forbid` — macros emit scoped #[allow]
#![deny(unreachable_pub)]
#![deny(clippy::all, clippy::pedantic)]
```

```rust
// tests/main.rs (the scaffolded runner, or your own)
#![allow(
    unreachable_pub,
    reason = "Suite/Test types must be pub so #[rudzio::suite] callsites can name them"
)]
#![allow(
    unused_crate_dependencies,
    reason = "test binary has a different dep set than the lib"
)]
#![allow(
    clippy::tests_outside_test_module,
    reason = "rudzio tests aren't inside the conventional #[cfg(test)] mod tests"
)]
```

`unsafe_code = "forbid"` at the lib level silently rejects rudzio's
scoped `#[allow(unsafe_code)]` for the `linkme` distributed-slice
registration — `forbid` doesn't accept downstream `allow` overrides.
Demote to `deny` if you want rudzio in. `rudzio-migrate` rewrites
`#![forbid(unsafe_code)]` → `#![deny(unsafe_code)]` in `src/lib.rs`
automatically when it detects suite-carrying modules.

## CLI flags

Every flag below is accepted in both `--flag=<val>` and `--flag <val>`
form (for flags that take a value). `<test-binary> --help` / `-h` prints
the same list at runtime.

### Filters and selection

| Flag | Purpose |
|---|---|
| `<FILTER>` (positional) | Substring match against test name — only tests whose name contains `<FILTER>` run. |
| `--skip <SUBSTRING>` | Exclude tests whose name contains `<SUBSTRING>`. Repeatable. |
| `--ignored` | Only run tests marked `#[ignore]`. |
| `--include-ignored` | Run every test, ignored or not. |
| `--list` | Print test names (one per line, libtest format) and exit without running anything. |

### Parallelism

| Flag | Purpose |
|---|---|
| `--test-threads <N>` | OS worker-thread count runtimes size their pool to. Defaults to `RUST_TEST_THREADS`, else `std::thread::available_parallelism()`. |
| `--concurrency-limit <N>` | Max in-flight tests per runtime group (scheduler knob; `--test-threads` is the executor knob). Defaults to `--test-threads` so a single `--test-threads=N` matches libtest's behaviour. |

### Output

| Flag | Purpose |
|---|---|
| `--format <pretty\|terse>` | Output format. Default: `pretty`. |
| `--color <auto\|always\|never>` | ANSI colour policy. Default: `auto`. |
| `--output <live\|plain>` | Rendering mode. `live` = bottom-of-terminal live region + history above (default on TTY with `CI` unset). `plain` = linear append-only (default off-TTY or under CI). |
| `--plain` | Shorthand for `--output=plain`. |

### Timeouts

| Flag | Purpose |
|---|---|
| `--test-timeout <SECS>` | Per-test budget. On expiry the per-test cancellation token fires and teardown still runs. Unbounded if unset. |
| `--run-timeout <SECS>` | Whole-run budget. Cancels the root token; in-flight tests cooperatively wind down, queued tests are reported as cancelled, teardowns run. Unbounded if unset. |

### Benchmarks

| Flag | Purpose |
|---|---|
| `--bench` | Dispatch `#[rudzio::test(benchmark=...)]` tests through their strategy. Non-bench tests still run. |
| `--no-bench` | Skip bench-annotated tests entirely (reported as ignored). |

### Help

| Flag | Purpose |
|---|---|
| `-h`, `--help` | Print usage and exit. |

### Environment variables

| Variable | Effect |
|---|---|
| `RUST_TEST_THREADS` | Default for `--test-threads` when the flag is absent. |
| `NO_COLOR` | If set (any value) and `--color=auto`, colour off. |
| `FORCE_COLOR` | If set (any value), colour on regardless of `--color` and TTY status. |
| `CI` | If set and `--output` is absent, selects `--output=plain` even on a TTY (CI log capture often can't render the live region). |

The full process environment is snapshotted into `Config::env` at
startup, so any `RUDZIO_*` convention your runtime wants to read is
already there. Unknown flags are preserved in `Config::unparsed` for
downstream parsing by custom runtimes or test helpers — they do not
produce an error.

### Exit status

| Code | Meaning |
|---|---|
| 0 | Every test passed (or none ran). |
| 1 | At least one test failed, panicked, was cancelled, or timed out; or a teardown failure fired. |
| 2 | Runner setup error (output capture init, etc.). |

## `Config` and runtime adaptation

`Config` is parsed once per invocation and handed to every runtime
constructor (`fn new(config: &Config) -> io::Result<Self>`) and to every
`Suite::setup` and `Suite::context`. Runtimes read what they need:

- `rudzio::runtime::tokio::Multithread` uses `config.threads` for
  `Builder::worker_threads`.
- `rudzio::runtime::futures::ThreadPool` uses `config.threads` for
  `ThreadPoolBuilder::pool_size`.
- Other built-in runtimes are single-threaded and ignore the threading
  knob; `Config` is still available via `Runtime::config(&self)` for
  test bodies.

Custom runtimes can go further: read `config.unparsed` for their own CLI
flags, or `config.env` for env-var-driven tuning — no need to extend
`Config` itself.

### `Config::cargo` (compile-time metadata)

`Config` carries a `CargoMeta` populated from `env!(...)` at the
`#[rudzio::main]` call site in the user's crate:

```rust
#[derive(Debug, Clone)]
pub struct CargoMeta {
    pub manifest_dir: PathBuf,  // env!("CARGO_MANIFEST_DIR")
    pub pkg_name: String,       // env!("CARGO_PKG_NAME")
    pub pkg_version: String,    // env!("CARGO_PKG_VERSION")
    pub crate_name: String,     // env!("CARGO_CRATE_NAME")
}
```

Use it to resolve fixture paths relative to the test crate without
calling `cargo` at runtime or parsing `Cargo.toml`:

```rust
let fixtures = ctx.config().cargo.manifest_dir.join("tests/fixtures");
```

Outside `#[rudzio::main]` (a unit test constructing a `Config` directly),
use `rudzio::cargo_meta!()`:

```rust
let config = rudzio::Config::parse(rudzio::cargo_meta!());
```

The macro expands to the `env!(...)` block at your call site, so the
captured values belong to your crate.

## Unsafe code

Rudzio forbids `unsafe_code` at the workspace level. The crate unlocks it
only where the problem genuinely requires it — each site has either a
module-level `#![allow(unsafe_code, reason = "…")]` with a one-line
justification or a per-site `#[allow(unsafe_code)]` adjacent to a
`// SAFETY:` comment. All of it falls into three categories:

1. **FFI for stdio capture** — `src/output/pipe.rs` (Unix-only,
   module-level allow). Wraps `libc::{dup, dup2, pipe, close, fcntl}` to
   save FDs 1 and 2, install anonymous pipes in their place, widen the
   kernel buffer via `F_SETPIPE_SZ`, and restore the originals on guard
   drop. What crosses the module boundary is a pair of safe `OwnedFd`
   read-ends and an atomic-backed `SavedFds` handle; the raw FDs never
   escape. A single per-site `ioctl(TIOCGWINSZ)` call each in
   `src/output/render.rs` and `src/runner.rs` reads the terminal width
   for right-aligning status lines — gated by `#[cfg(unix)]` and
   bracketed by `#[allow(unsafe_code)]`, with a 100-column fallback on
   failure.

2. **Embassy executor glue** — `src/runtime/embassy/runtime.rs`
   (module-level allow). Embassy's `raw::Executor` is a pointer-threaded,
   no-alloc API; running it inside a hosted runtime requires casting a
   `*mut ()` context back to `&'static Signaler` in the mandatory
   `__pender` export, extending the lifetime of a pinned future to
   `'static` for the span of a `block_on` call, and one
   `unsafe impl Send for SlotPtr<T>` around the output slot. Each block
   carries a SAFETY comment tying its reasoning to the `block_on`
   synchronisation. The rest of the crate sees the safe `Runtime` trait
   impl only.

3. **Macro-generated test dispatch** — every `TestToken` carries a
   `for<'s> unsafe fn` pointer (`TestRunFn` in `src/suite.rs`). The macro
   emits both the caller (`RuntimeGroupOwner::run_group`) and the callee
   (the per-test fn that casts `runtime_ptr` / `suite_ptr` back to
   concrete `&R` / `&Suite`). Soundness rides on the compile-time
   `runtime_group_key` hash matching on both sides — the macro emits both
   sides, so a mismatch is a macro bug, not a user footgun. Generated
   `unsafe { … }` blocks are `#[allow(unsafe_code)]` locally with a
   matching SAFETY comment in the codegen.

Outside these three places — user-visible test bodies, `Suite` impls,
`Runtime` impls, custom `Strategy` implementations — the workspace's
deny-unsafe lint applies unchanged. Nothing a user writes to drive rudzio
requires unsafe.

## Sharp edges

- **Tokio version pin.** Rudzio currently pins `tokio = "=1.52.1"`
  exactly. Downstream crates with a different locked version will fail
  resolution. Workaround: `cargo update -p tokio --precise 1.52.1` in
  the downstream crate. The exact pin will relax once the tokio APIs
  rudzio uses are stable across a broader range.

- **`#[dtor]`-using crates can eat rudzio's output.** If any dep uses the
  `dtor` crate to run cleanup at process exit and that destructor panics,
  the process aborts with `SIGABRT` *after* rudzio has printed its
  summary — but before the terminal has flushed the full output. Not a
  rudzio bug; the summary lines are issued, the shell just loses them.
  Wrap the `#[dtor]` body in a panic guard if you own the destructor.

- **Parallel suite startup fan-out.** The runner spawns one OS thread per
  `(runtime, suite)` group and calls `Suite::setup` on each in parallel.
  If those setups start Docker/podman containers, 4–5 simultaneous
  container starts can tickle hyper connection issues on podman
  specifically (`IncompleteMessage`, `WaitContainer(StartupTimeout)`).
  Workaround: wrap the container start in a global
  `tokio::sync::Mutex<()>` inside your own `Suite::setup`. A future
  release may expose a `--max-concurrent-suites N` knob.

- **Dev-dep cycles break regime 3.** If member A has B in
  `[dev-dependencies]` and B has A in `[dependencies]`, the generated
  bridge for A carries B in its own `[dependencies]` — and cargo
  rejects the resulting cycle. Cargo tolerates cycles only across
  `[dev-dependencies]` edges; bridges put those edges into
  `[dependencies]`. Rearrange dev-deps or move the affected test into
  `tests/*.rs` (integration tests compile into the aggregator's own
  unit and bypass the bridge).

## Migrating an existing suite

The workspace ships [`rudzio-migrate`](migrate/README.md), a CLI that
converts cargo-style `#[test]` / `#[tokio::test]` / `#[test_context(T)]`
suites into rudzio shape. It runs on a clean git tree, rewrites sources
in place, keeps a per-file backup plus a `/* pre-migration */` block
comment above every converted fn, wires `Cargo.toml` for both the `[lib]
harness = false` unit-test path and the `[[test]] harness = false`
integration-test path, and appends `#[cfg(test)] #[rudzio::main] fn
main() {}` to `src/lib.rs` when src-resident unit tests are involved.

```sh
cargo rudzio migrate --path /path/to/your/crate
```

Preconditions: clean git tree, and an acknowledgement phrase typed
verbatim. No `--force`, no `--yes`. Migration output is not guaranteed to
compile — on a non-trivial codebase, expect most tests to convert on the
first pass and a small number of warnings flagged at file:line. Review
the result via `git diff`; per-file backups
(`<file>.backup_before_migration_to_rudzio`) are written alongside each
rewritten source.

Scope, limits, and recipe: [`migrate/README.md`](migrate/README.md).

## Authorship

Design — trait shape, lifetimes, dispatch model, output format, scope —
is by the author. A significant portion of the source was typed by an
LLM under supervision: boilerplate, macro expansions, documentation,
mechanical refactors. Architecture and decisions are not.

The `Suite` / `Test` split adapts the pattern from the
[`test-context`][test-context] crate into async and adds a suite-level
lifetime scope. Tokio, compio, embassy, futures-executor, libtest,
linkme, signal-hook, and crossbeam are load-bearing dependencies.

[test-context]: https://crates.io/crates/test-context

## License

MIT or Apache-2.0, at your option.

---

Named after Rudzisław, an orange cat (—2025-12-31).
