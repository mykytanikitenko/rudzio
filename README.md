# rudzio

Async test framework for Rust with pluggable runtimes and per-test
`setup`/`teardown`. Each test runs against a fresh test context that you build
on top of a shared per-suite context. Cancellation, per-test and per-run
timeouts, panic isolation, and SIGINT/SIGTERM handling are all wired up by the
runner.

## When to use it

- You're testing async code and `#[tokio::test]` macros aren't expressive
  enough — you want shared expensive setup (DB pool, container, server) created
  once per group, with per-test scopes underneath.
- You want the same test bodies to run against multiple async runtimes
  (tokio multi-thread, tokio current-thread, tokio LocalRuntime, compio,
  embassy) without rewriting them.
- You want graceful shutdown — root-token cancellation on SIGINT/SIGTERM/run
  timeout that fans out into in-flight tests, with teardown still running.

If you only need an async unit test, `#[tokio::test]` is fine. Reach for this
when the lifecycle around the test matters as much as the test body.

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
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", features = ["runtime-tokio-multi-thread", "common"] }
```

Features are all off by default. Pick what you need:
- `common` — ready-made `Suite`/`Test` pair on top of `CancellationToken` +
  `TaskTracker` at `rudzio::common::context`. Omit if you're writing your own
  context types.
- `runtime-tokio-multi-thread` — `rudzio::runtime::tokio::Multithread` (tokio's
  multi-thread runtime; pulls in `tokio/rt-multi-thread`).
- `runtime-tokio-current-thread` — `rudzio::runtime::tokio::CurrentThread`
  (tokio's current-thread runtime + `LocalSet`; `!Send` futures via a
  `SendWrapper` shim).
- `runtime-tokio-local` — `rudzio::runtime::tokio::Local` (tokio's
  `LocalRuntime`; native `!Send` support in `block_on`/`spawn_local`).
- `runtime-compio` — `rudzio::runtime::compio::Runtime`.
- `runtime-embassy` — `rudzio::runtime::embassy::Runtime`.
- `runtime-futures` — `rudzio::runtime::futures::ThreadPool` (on top of
  `futures::executor::ThreadPool`).

The three `runtime-tokio-*` features are independent — enable only the
flavours you actually use, or enable all three to get every
`rudzio::runtime::tokio::*` type.

`harness = false` is required — the `#[rudzio::main]` attribute installs the
runner that walks every `#[rudzio::test]` registered via `linkme`. Not yet on
crates.io; pin to a commit in your `Cargo.toml` if you care about
reproducibility.

### Lib unit tests (no `tests/` directory)

If your tests live inside `src/` under `#[cfg(test)] mod tests { ... }`
blocks — the classic rust-lang unit-test shape — you can run them
through rudzio without adding a `tests/` directory at all. Two edits:

```toml
# Cargo.toml
[lib]
harness = false            # opt out of libtest for the lib's own test target

[dev-dependencies]
rudzio = { version = "0.1", features = ["runtime-tokio-multi-thread", "common"] }
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

// Entry point for the lib's test target (harness = false). The
// cfg(test) gate keeps it out of downstream binaries that depend on
// this lib.
#[cfg(test)]
#[rudzio::main]
fn main() {}
```

`cargo test --lib` now runs through `#[rudzio::main]` — no separate
aggregator, no `#[path]` trickery. `rudzio-migrate` (below) wires both
edits automatically when it converts a crate with `src/` unit tests.

### Return type: any `Result<T, E: Display>`

`#[rudzio::test]` test bodies must return a `Result`. The error type is
anything that implements `Display + Debug + Send + Sync` — the runner
calls `.map_err(|e| format!("{e}"))` on whatever you hand back. So all
of these work:

```rust
#[rudzio::test]
async fn uses_anyhow() -> anyhow::Result<()> { Ok(()) }

#[rudzio::test]
async fn uses_n0_snafu() -> n0_snafu::Result { Ok(()) }

#[rudzio::test]
async fn uses_std_io() -> Result<(), std::io::Error> { Ok(()) }

#[rudzio::test]
async fn uses_custom_enum() -> Result<(), MyError> { Ok(()) }
```

Bare `fn foo() {}` does not compile — the macro can't add an error tail
when there's no `Result` to map over. If you've got hundreds of
`()`-returning tests to migrate, `rudzio-migrate` rewrites the
signatures automatically.

### The context parameter is optional

Zero-argument test bodies are a first-class shape; the macro fills in
the per-test context at expansion time. Suite setup and per-test
teardown still run.

```rust
#[rudzio::test]
async fn doesnt_need_the_context() -> anyhow::Result<()> {
    // ...no `ctx: &Test` parameter — still a real test.
    Ok(())
}
```

## Examples

Four runnable examples in `examples/` cover the common shapes:

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

Three traits, three lifetimes, in strict outer-to-inner order:

```
'runtime  >  'suite_context  >  'test_context
```

| Trait                              | Lives for        | Created                           | Dropped                          |
|------------------------------------|------------------|-----------------------------------|----------------------------------|
| `Runtime<'rt>`                     | `'runtime`       | once per `(runtime, suite)` group | when the group thread exits     |
| `Suite<'suite_context, R>`         | `'suite_context` | once per group, after `Runtime`   | after the last test in the group |
| `Test<'test_context, R>`           | `'test_context`  | once per test, in `Suite::context`| after the test body returns     |

`Self::Test` on `Suite` is a GAT — `Self::Test<'test_context>` — so the
per-test context value genuinely lives in the per-test borrow lifetime, not
in the suite's. That's what makes `&mut TestCtx` parameters compile.

`#[rudzio::test]` accepts `&Ctx`, `&mut Ctx`, or no parameter at all as
the first argument, sync or async body, returning `anyhow::Result<()>`
(or any `Display`-able error wrapped in `Result`). Zero-arg test bodies
still see full per-test setup + teardown — they just don't receive the
context.

## Multiple runtimes per test

Each tuple in the `#[rudzio::suite]` list is a separate `(runtime, suite)`
configuration. The same test bodies run against each of them:

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
`#[rudzio::suite]` blocks declaring the same `(runtime, suite)` collapse into
one thread / one runtime / one suite instance — keyed by a compile-time hash
of the `(runtime_path, suite_path)` token strings.

## Runtimes

Behind feature flags. Default: none — pick what you need.

- `runtime-tokio-multi-thread` → `rudzio::runtime::tokio::Multithread`
- `runtime-tokio-current-thread` → `rudzio::runtime::tokio::CurrentThread`
- `runtime-tokio-local` → `rudzio::runtime::tokio::Local`
- `runtime-compio` → `rudzio::runtime::compio::Runtime`
- `runtime-embassy` → `rudzio::runtime::embassy::Runtime`

Implementing your own `Runtime<'rt>` is a regular trait impl; nothing in the
runner is hard-coded to a specific runtime crate.

## Custom contexts

`rudzio::common::context` ships a ready-to-use `(Suite, Test)` pair on top of
`tokio_util::sync::CancellationToken` + `tokio_util::task::TaskTracker` (enable
the `common` feature). If you need your own (a `sqlx::PgPool`, an HTTP server
handle, a mock clock), define structs that implement `rudzio::context::Suite`
and `rudzio::context::Test`. See `fixtures/src/bin/custom_context_tokio_mt.rs` for a
minimal hand-rolled example.

## Same-crate single-binary test runner (unit + integration + e2e)

If you want every flavour of test in *one* crate — unit tests inside
`src/`, integration tests in `tests/integration/`, e2e tests behind a
feature flag — scheduled by one `#[rudzio::main]`, you can do it
without a separate aggregator crate:

```toml
# Cargo.toml
[lib]
harness = false

[features]
integration = []
e2e         = []

[dev-dependencies]
rudzio = { version = "0.1", features = ["runtime-tokio-multi-thread", "common"] }
```

```rust
// src/lib.rs
#[cfg(test)]
extern crate self as my_crate;         // crate:: paths in the includes resolve to this binary

// Unit tests inside `src/` register normally via their own
// `#[cfg(test)] #[rudzio::suite(...)] mod tests { ... }` blocks —
// nothing to do here.

// Integration tests: pull `tests/integration/mod.rs` into the lib's
// test target so their `linkme` entries register in the same slice.
#[cfg(all(test, feature = "integration"))]
#[path = "../tests/integration/mod.rs"]
mod integration;

// Same for e2e behind another feature flag.
#[cfg(all(test, feature = "e2e"))]
#[path = "../tests/e2e/mod.rs"]
mod e2e;

#[cfg(test)]
#[rudzio::main]
fn main() {}
```

`cargo test --lib` runs unit tests. `cargo test --lib --features
integration` adds integration. `cargo test --lib
--features integration,e2e` adds e2e. All three flavours share the
same `#[rudzio::main]` binary and the same scheduler pass.

### Spawning this crate's own `[[bin]]` targets

If the tests wired up above spawn this crate's `[[bin]]`s as child
processes, use `rudzio::bin!` at every call site:

```rust
let mut server = std::process::Command::new(rudzio::bin!("my-server"))
    .arg("--port=0")
    .spawn()?;
```

The macro returns `std::path::PathBuf` and works identically across
every rudzio layout.

Unlike `tests/*.rs` integration tests, `cargo test --lib` does **not**
populate `CARGO_BIN_EXE_<name>` — cargo only auto-wires that env var
for integration-test binaries, and the `--lib` test binary isn't one.
So `rudzio::bin!` has to find the bin another way. You've got two
choices:

- **Pre-build.** Run `cargo build --bins` before `cargo test --lib`.
  `rudzio::bin!` walks up from `std::env::current_exe()` to
  `target/<profile>/<name>` at runtime; if the file exists, it's
  returned.
- **Auto-build via `build.rs`.** A 3-line build script asks rudzio
  to build this crate's own bins into a sandboxed cache and emit
  `CARGO_BIN_EXE_<name>` for each, which `rudzio::bin!` then picks
  up at compile time:

  ```rust
  // build.rs
  fn main() -> Result<(), rudzio::build::Error> {
      rudzio::build::expose_self_bins()
  }
  ```

  ```toml
  # Cargo.toml
  [build-dependencies]
  rudzio = { version = "0.1", default-features = false, features = ["build"] }
  ```

  `expose_self_bins` reads `CARGO_PKG_NAME` and delegates to
  `expose_bins` (see the aggregator section below). A re-entry
  sentinel stops the build script from recursing into itself when the
  nested `cargo build --bins -p <self>` re-runs this same build
  script.

If neither path has produced the binary by the time `rudzio::bin!` is
evaluated, it panics with a message naming the bin and pointing at
the fix.

## Borrowing from the `Suite` (HRTB asymmetry)

`Suite<'suite_context, R>` requires `R: for<'r> Runtime<'r>` (the
runtime is reused across every per-test borrow), while `Test<'test_context, R>`
requires only `R: Runtime<'test_context>` (one specific lifetime).
The asymmetry is deliberate — it's what lets a test hold a borrow of
the suite without dragging the HRTB bound onto the test type — but it
has a sharp edge:

```rust
// Hold `&Suite` in Test → HRTB requirement bubbles up onto Test's R,
// and the `#[rudzio::test]` macro's call site resolves R under the
// *non*-HRTB bound → `trait bound for<'__r> R: rudzio::Runtime<'__r>
// is not satisfied` at the test fn.
pub struct MyTest<'test_context, R> {
    pub suite: &'test_context MySuite<'test_context, R>,   // ✗ propagates HRTB
}
```

The workaround: have the `Test` borrow *specific fields* of the suite,
not `&Suite` itself. Nothing in the rudzio API asks for `&Suite` —
`Suite::context` decides what the per-test value contains:

```rust
pub struct MyTest<'test_context> {
    pub pool: &'test_context sqlx::PgPool,   // ✓ bare borrow, no R propagation
}
```

When the test only needs a couple of suite-owned handles, threading
those through avoids the HRTB mismatch entirely and keeps the
generated macro call site simple.

## Recommended lint configuration

If your crate runs with strict lints, a few defaults need relaxing for
rudzio's macros and test binaries. Suggested starting point:

```rust
// src/lib.rs (your crate)
#![deny(unsafe_code)]                   // not `forbid` — macros emit scoped #[allow]
#![deny(unreachable_pub)]               // keep normal, but…
#![deny(clippy::all, clippy::pedantic)] // whatever your baseline is
```

```rust
// tests/main.rs (the scaffolded runner, or your own)
#![allow(
    unreachable_pub,
    reason = "Suite/Test types must be pub so `#[rudzio::suite]` callsites can name them"
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

The pitfall worth naming: `unsafe_code = "forbid"` at the lib level
silently rejects rudzio's scoped `#[allow(unsafe_code)]` for the
`linkme` distributed-slice registration — forbid doesn't accept
downstream `allow` overrides. Demote to `deny` if you want rudzio
in. `rudzio-migrate` rewrites `#![forbid(unsafe_code)]` →
`#![deny(unsafe_code)]` in `src/lib.rs` automatically when it detects
suite-carrying modules.

## Workspace-wide single-binary test runner

Tests live in `tests/*.rs` (per-crate integration tests), so
`cargo test -p <crate>` works the way you'd expect. If you also want a single
binary that runs every crate's tests in one process — one runtime per
`(runtime, suite)` tuple, one scheduler, one pass of output, tests grouped
and deduped by the `RuntimeGroupKey` hash — the supported path is:

```
cargo rudzio test [ARGS...]
```

`cargo-rudzio` (install with `cargo install cargo-rudzio`, or use
`cargo run -p cargo-rudzio --` inside this repo) inspects the workspace,
generates an aggregator crate in `<target-dir>/rudzio-auto-runner/` on every
invocation, builds it, and runs it. The aggregator depends on every rudzio-
using member, `#[path]`-includes each member's `tests/*.rs`, and drives
everything under one `#[rudzio::main]`. ARGS are forwarded to the aggregator
binary — filter patterns, `--skip`, `--bench`, all the rudzio config flags.

A bin-owning member's `[[bin]]` targets are re-built and exposed via
`CARGO_BIN_EXE_<name>` automatically (the generated aggregator's `build.rs`
handles this, per member), so tests that spawn sibling binaries keep
working. The fallback `rudzio::bin!` / runtime-walk is what makes test
code portable across per-crate and aggregated modes.

### Hand-rolling an aggregator

If you need to customize the aggregator (e.g. carry different feature
selections per runtime, or preprocess test files) you can still hand-roll
the `test-runner` crate. The auto-generator just produces the same shape:

`test-runner/Cargo.toml`:
```toml
[package]
name = "test-runner"
edition = "2024"

# Own (single-crate) workspace so this crate's feature selections do NOT
# unify into the parent workspace's other binaries.
[workspace]

[dependencies]
rudzio = { path = "..", features = ["runtime-tokio-multi-thread", "common"] }
my-crate = { path = "../my-crate" }
anyhow = "1"
```

`test-runner/src/main.rs`:
```rust
mod tests;

#[rudzio::main]
fn main() {}
```

`test-runner/src/tests/mod.rs`:
```rust
mod my_crate;
```

`test-runner/src/tests/my_crate.rs`:
```rust
#[path = "../../../my-crate/tests/it.rs"]
mod it;
```

Compile + run: `(cd test-runner && cargo run)`. Every `#[rudzio::test]` from
every included file registers through `linkme` into the binary's single
`TEST_TOKENS` slice; the runner filters / dispatches / reports them all
together.

One thing to watch:

- **Feature unification.** Cargo unifies features across every workspace
  member's deps. If your aggregator requests features on `rudzio` that
  the sibling crates don't already have on, those features activate
  everywhere. Keep the aggregator's feature list the same as the
  sibling crates', or exclude the aggregator from the parent workspace
  (`[workspace] exclude = ["test-runner"]`) if you need it to carry a
  wholly different set. In this repo the feature lists line up, so
  `test-runner/` lives inside the parent workspace without issues.

### Resolving `[[bin]]` paths from tests: `rudzio::bin!`

`rudzio::bin!("<name>")` returns a `PathBuf` to the named bin. It's
the one call site you use in every layout — `tests/*.rs` integration
tests, shared aggregators, `cargo test --lib` runners — so test code
doesn't need to branch on which one it's running under:

```rust
let mut child = std::process::Command::new(rudzio::bin!("my-server"))
    .arg("--port=0")
    .spawn()?;
```

Resolution chain:

1. `option_env!(concat!("CARGO_BIN_EXE_", <name>))` at compile time.
   Cargo populates this automatically for `tests/*.rs` integration
   tests of the crate that declares the `[[bin]]`; rudzio's
   `expose_bins` / `expose_self_bins` build-script helpers populate
   it for the shared-aggregator and `cargo test --lib` layouts.
2. Runtime walk from `std::env::current_exe()` to
   `target/<profile>/<name>` if step 1 missed. Covers the
   `cargo test --lib` layout when the user pre-builds with
   `cargo build --bins` instead of adding a build script.
3. Panic naming the bin and pointing at the two fixes (pre-build, or
   add `build.rs` with `expose_self_bins`) if both miss.

### Aggregating tests that spawn `[[bin]]` targets

Tests that `#[path]`-include into an aggregator crate lose access to
`CARGO_BIN_EXE_<name>`: cargo only populates those for integration
tests of the crate that declares the `[[bin]]`. Dropping
`rudzio::bin!` into the call site isn't enough on its own — the
aggregator's build has nothing to walk up to either.

Rudzio ships `rudzio::build::expose_bins` (behind the `build` feature)
for exactly this. Call it once in your aggregator's `build.rs`:

```toml
# test-runner/Cargo.toml
[dependencies]
my-bin-crate = { path = "../my-bin-crate" }

[build-dependencies]
rudzio = { version = "0.1", default-features = false, features = ["build"] }
```
```rust
// test-runner/build.rs
fn main() -> Result<(), rudzio::build::Error> {
    rudzio::build::expose_bins("my-bin-crate")
}
```

What happens: the build script reads `cargo metadata` for `my-bin-crate`,
runs `cargo build --bins -p my-bin-crate` with a dedicated target dir
(`$OUT_DIR/rudzio-bin-cache`, so there's no lock contention with the outer
cargo), and emits `cargo:rustc-env=CARGO_BIN_EXE_<name>=<abs path>` for each
bin. The `rudzio::bin!("<name>")` call sites in the `#[path]`-included
tests then resolve via step 1 of the chain above, exactly as they did in
the bin crate's own integration tests.

For the "this crate's bins for its own `cargo test --lib` runner" case
(see the `Same-crate single-binary test runner` section), use
`rudzio::build::expose_self_bins()` — it reads `CARGO_PKG_NAME` and
delegates to `expose_bins`, so you don't hardcode your own crate name.

A re-entry sentinel (`__RUDZIO_EXPOSE_BINS_ACTIVE`, set on every
nested cargo) breaks the recursion that would otherwise happen when
the nested `cargo build --bins -p <self>` re-runs the same build
script: the second entry short-circuits to `Ok(())`. This makes
`expose_self_bins()` safe despite the nested-cargo pattern.

Dep requirement: the bin crate must have a lib target (even empty
`src/lib.rs` is fine) so Cargo accepts it as a regular dep. Everything else
— feature resolution, profile (`--release` forwarded from the outer
`PROFILE`), rerun-on-change hooks — `rudzio::build` handles.

No fallbacks: missing env var, missing package in metadata, missing bin,
nested build failure → build script errors with an explicit message.

Rudzio's own workspace demonstrates every mode side-by-side:
- `cargo test --workspace` runs each crate's per-crate tests (143 tests
  across rudzio / macro-internals / e2e).
- `cargo run -p rudzio-test-runner` aggregates **all** of them into one
  binary — 96 rudzio dogfood tests (exercised under all 6 runtimes) + 17
  macro-internals parser tests + 29 e2e integration tests, all scheduled
  by one `#[rudzio::main]`. `test-runner/build.rs` uses
  `rudzio::build::expose_bins("rudzio-fixtures")` to make the 30+ fixture
  binaries reachable from the `rudzio::bin!("<name>")` call sites in
  those tests.

## Benchmarks

Any `#[rudzio::test]` can also run as a benchmark by adding a
`benchmark = <strategy>` argument to the attribute. Without `--bench`,
the body runs exactly once as a regular test (smoke mode is the default
— the `benchmark = ...` expression isn't even evaluated). With
`--bench`, the runner dispatches through the strategy, collects
per-iteration timings, and prints a distribution.

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
repeat-K-rounds, rate-limit-to-X-rps — whatever) by writing a new impl.
The attribute's argument is just a Rust expression, so the macro
evaluates whatever value you give it at the call site — no registry, no
magic.

`--bench`-mode output includes a one-line `[BENCH]` status with the
headline figures, a detailed multi-line statistics block (sample count,
wall-clock, throughput, min/max/range, mean, median, σ, MAD,
coefficient of variation, IQR, outlier count, and percentiles from p1
through p99.9), and an ASCII histogram. Bench-annotated tests with
`&mut Ctx` are rejected at macro time because the strategy calls the
body repeatedly (Concurrent would clash on the exclusive borrow).

CLI flags:

- *(default)* — smoke mode: body runs once, regardless of
  `benchmark = ...`.
- `--bench` — full mode: run every bench-annotated test through its
  strategy. Non-bench tests still run.
- `--no-bench` — skip mode: bench-annotated tests are reported as
  ignored (handy on slow CI).

### Precision expectations

Rudzio's benchmark facility is built for *rational estimation*, not
nanosecond-grade precision. Per-iteration measurement wraps the body
in `AssertUnwindSafe + catch_unwind`, reads `Instant::now()` before
and after, and pushes a `Duration` into a `Vec` — a pipeline whose
own cost sits in the tens of nanoseconds, before you add the async
runtime's polling and waker overhead. The strategies also do none of
the things a serious nanosecond harness would do: no adaptive
iteration budget, no warm-up passes, no linear regression against
iteration count to subtract fixed overhead, no Tukey-style outlier
rejection, no CPU pinning, no `std::hint::black_box` to defend the
body from constant-folding.

In practice that means the numbers are useful when the signal sits
comfortably above the measurement floor:

- **Regression tracking.** Yesterday's `Sequential(1000)` p50 was
  45 µs; today it's 70 µs — that's a real signal worth chasing.
- **Distribution-shape comparison.** Run the same body under
  `Sequential(N)` and `Concurrent(N)` and watch how p99 and the
  coefficient of variation diverge — what you learn about your
  async scheduling under contention is real.
- **Integration-level work.** End-to-end flows involving IO,
  allocation, or cross-thread coordination, where per-iteration
  cost is orders of magnitude above the instrument's own overhead.

If you need single-nanosecond precision — micro-benchmarks of hot
inner loops, compiler-optimized paths, cache-sensitive numeric
kernels — reach for [criterion][criterion] or a direct perf-counter
harness. Rudzio will tell you the wrong thing with confidence at
that scale.

[criterion]: https://crates.io/crates/criterion

## Cancellation, timeouts, panics

- **`--test-timeout=N`** (seconds): per-test budget. On expiry the per-test
  cancellation token is cancelled (test body sees the signal) and teardown
  still runs.
- **`--run-timeout=N`** (seconds): whole-run budget. Cancels the root token;
  in-flight tests cooperatively wind down, queued tests are reported as
  cancelled, teardowns run.
- **SIGINT / SIGTERM**: same as run timeout. No `kill -9`-shaped exits as long
  as your tests respect their cancellation token.
- **Panics**: each test body and every teardown is wrapped in `catch_unwind`.
  A panicking test is counted as `FAILED (panicked)`; a panicking teardown is
  logged as a warning but doesn't take down the run.

Each suite group gets a child of the run-wide root token, so a `Suite::teardown`
that cancels its stored token only fans out within its own group.

## CLI flags (libtest-compatible subset)

```
<filter>                    positional substring match against test name
--skip <s> / --skip=<s>     exclude tests whose name contains <s>
--ignored                   only run #[ignore]d tests
--include-ignored           run every test, ignored or not
--bench                     dispatch #[rudzio::test(benchmark=...)] tests through their strategy
--no-bench                  skip #[rudzio::test(benchmark=...)] tests entirely
--list                      list test names and exit
--test-threads=N            OS worker-thread count runtimes size their pool to
--concurrency-limit=N       max in-flight tests per group (defaults to --test-threads)
--format=pretty|terse       output style
--color=auto|always|never   colour control
--test-timeout=N            per-test timeout (seconds)
--run-timeout=N             whole-run timeout (seconds)
```

`RUST_TEST_THREADS=N` and `NO_COLOR=1` are honoured.

Unknown flags aren't errors — they're preserved in `Config::unparsed` so
custom runtimes or test helpers can parse them. The full process
environment is snapshotted into `Config::env` at startup, so any
`RUDZIO_WHATEVER` convention your runtime wants to read is already there.

## `Config` and runtime adaptation

`Config` is parsed once per invocation and handed to every runtime
constructor (`fn new(config: &Config) -> io::Result<Self>`) and to every
`Suite::setup` and `Suite::context`. Runtimes can read what they need.
Today:

- `rudzio::runtime::tokio::Multithread` — uses `config.threads` for
  `Builder::worker_threads`.
- `rudzio::runtime::futures::ThreadPool` — uses `config.threads` for
  `ThreadPoolBuilder::pool_size`.
- All other built-in runtimes are single-threaded and ignore the
  threading knob; `Config` is still available via `Runtime::config(&self)`
  for test bodies.

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

Outside `#[rudzio::main]` (say, in a unit test that constructs a
`Config` directly), use `rudzio::cargo_meta!()`:

```rust
let config = rudzio::Config::parse(rudzio::cargo_meta!());
```

The macro expands to the `env!(...)` block at *your* call site, so the
captured values belong to your crate.

## Unsafe code

rudzio forbids `unsafe_code` at the workspace level. The crate unlocks
it only where the problem genuinely requires it — each site has either
a module-level `#![allow(unsafe_code, reason = "…")]` with a one-line
justification or a per-site `#[allow(unsafe_code)]` adjacent to a
`// SAFETY:` comment explaining why the call is sound. All of it falls
into three categories:

1. **FFI for stdio capture** — `src/output/pipe.rs` (Unix-only,
   module-level allow). Wraps `libc::{dup, dup2, pipe, close, fcntl}`
   to save FDs 1 and 2, install anonymous pipes in their place, widen
   the kernel buffer via `F_SETPIPE_SZ`, and restore the originals on
   guard drop. What crosses the module boundary is a pair of safe
   `OwnedFd` read-ends and an atomic-backed `SavedFds` handle; the
   raw FDs never escape. A single per-site `ioctl(TIOCGWINSZ)` call
   each in `src/output/render.rs` and `src/runner.rs` reads the
   terminal width for right-aligning status lines — gated by
   `#[cfg(unix)]` and bracketed by `#[allow(unsafe_code)]`, with a
   100-column fallback on failure.

2. **Embassy executor glue** — `src/runtime/embassy/runtime.rs`
   (module-level allow). Embassy's `raw::Executor` is a
   pointer-threaded, no-alloc API; running it inside a hosted
   runtime requires casting a `*mut ()` context back to
   `&'static Signaler` in the mandatory `__pender` export,
   extending the lifetime of a pinned future to `'static` for the
   span of a `block_on` call, and one `unsafe impl Send for
   SlotPtr<T>` around the output slot. Each block carries a
   SAFETY comment tying its reasoning to the `block_on`
   synchronisation. The rest of the crate sees the safe
   `Runtime` trait impl only.

3. **Macro-generated test dispatch** — every `TestToken`
   carries a `for<'s> unsafe fn` pointer (`TestRunFn` in
   `src/suite.rs`). The macro emits both the caller
   (`RuntimeGroupOwner::run_group`) and the callee (the per-test
   fn that casts `runtime_ptr` / `suite_ptr` back to concrete
   `&R` / `&Suite`). Soundness rides on the compile-time
   `runtime_group_key` hash matching on both sides — the macro
   emits both sides, so a mismatch is a macro bug, not a user
   footgun. Generated `unsafe { … }` blocks are
   `#[allow(unsafe_code)]` locally with a matching SAFETY
   comment in the codegen.

Outside these three places — user-visible test bodies, `Suite`
impls, `Runtime` impls, custom `Strategy` implementations — the
workspace's deny-unsafe lint applies unchanged. Nothing a user
writes to drive rudzio requires unsafe.

## Sharp edges

A handful of things worth knowing before they bite:

- **Tokio version pin.** Rudzio currently pins `tokio = "=1.52.1"` exactly.
  Downstream crates with a different locked version will fail resolution.
  Workaround: `cargo update -p tokio --precise 1.52.1` in the downstream
  crate. The exact pin will relax once we're confident the `tokio` APIs
  rudzio uses are stable across a broader range.

- **`#[dtor]`-using crates can eat rudzio's output.** If any dep uses
  the `dtor` crate to run cleanup at process exit (e.g. container
  reapers in test utilities) and that destructor panics, the process
  aborts with `SIGABRT` *after* rudzio has printed its summary — but
  before the terminal has flushed the full output. Not a rudzio bug;
  the summary lines are issued, the shell just loses them. Wrap the
  `#[dtor]` body in a panic guard if you own the destructor.

- **Parallel suite startup fan-out.** The runner spawns one OS thread
  per `(runtime, suite)` group and calls `Suite::setup` on each in
  parallel. If those setups start Docker/podman containers, 4–5
  simultaneous container starts can tickle hyper connection issues on
  podman specifically (`IncompleteMessage`,
  `WaitContainer(StartupTimeout)`). Workaround: wrap the container
  start in a global `tokio::sync::Mutex<()>` inside your own
  `Suite::setup`. A future release may expose a
  `--max-concurrent-suites N` knob.

## Migrating an existing suite

The workspace ships [`rudzio-migrate`](migrate/README.md), a CLI that
converts cargo-style `#[test]` / `#[tokio::test]` / `#[test_context(T)]`
suites into rudzio shape. It runs on a clean git tree, rewrites sources
in place, keeps a per-file backup plus a `/* pre-migration */` block
comment above every converted fn, wires `Cargo.toml` for both the
`[lib] harness = false` unit-test path and the `[[test]] harness =
false` integration-test path, and appends `#[cfg(test)] #[rudzio::main]
fn main() {}` to `src/lib.rs` when src-resident unit tests are involved.

```sh
cargo install --path cargo-rudzio
cargo rudzio migrate --path /path/to/your/crate
```

Or invoke the binary directly: `cargo install --path migrate` then
`rudzio-migrate --path ...`. Both paths drive the same `run::entry`,
so behaviour and flags are identical.

## The `cargo rudzio` CLI

Installs as a Cargo subcommand:

```sh
cargo install --path cargo-rudzio
```

Subcommands:

- `cargo rudzio test [ARGS...]` — generates + runs the workspace
  aggregator (see the "Workspace-wide single-binary test runner"
  section). One binary, one `#[rudzio::main]`, every test from every
  crate grouped by `(runtime, suite)`. ARGS pass through to the
  aggregator binary, which accepts the full rudzio config flag set
  (filter patterns, `--skip`, `--bench`, `--format`, `--threads`, …).

- `cargo rudzio migrate [ARGS...]` — shortcut for the `rudzio-migrate`
  binary. All migrator flags work (`--path`, `--runtime`, `--dry-run`,
  `--no-shared-runner`, `--no-preserve-originals`, `--tests-only`,
  `--only-package`).

- `cargo rudzio generate-runner [--output DIR]` — regenerates the
  aggregator crate without running it. Useful for inspecting the
  generated `Cargo.toml` / `src/main.rs` / `build.rs` before
  committing to cargo rudzio as your primary test driver.

Two hard gates — clean git tree, acknowledgement phrase — then it runs.
No `--force`, no `--yes`. Explicit honesty up front: the output is not
guaranteed to compile. On the author's own real-world migration
targets the realistic outcome is "most tests compile on the first try,
a short warning list, a handful of manual fix-ups spotted via `git
diff`". The friction is the point; the tool is a mechanical stepping
stone, not a replacement for reading the diff.

Full scope table, known limits, and the recipe are in
[`migrate/README.md`](migrate/README.md). The full note on how
`rudzio-migrate` came to exist — including the fact that it's itself
heavily AI-assisted, written because the author needed at least
partial automation for the rudzio rollout on a real codebase and
needed results fast — lives in that README.

## Status

`0.1.x`. The shape of `Suite`, `Test`, `Runtime` and the suite macro is
intentionally kept stable — there are tests asserting on the rendered output
format and on cancellation/teardown behaviour. Internals (`SuiteRunner`,
`TestToken` layout, `RuntimeGroupKey` hashing) are `#[doc(hidden)]` and may
change.

## Acknowledgements, and honest notes on authorship

### Division of labour

The idea, the architectural shape, the type and lifetime design, the
API surface, and the scope discipline are mine. So is the taste behind
the specific decisions — `'runtime: 'suite_context: 'test_context`,
the HRTB `unsafe fn` dispatch model, the `RuntimeGroupOwner`
coalescing scheme, the choice to render a live region rather than a
JSON stream, the no-panic rule enforced across library code, the
decision to forbid unsafe outside three localised places. Each of
those survived me pushing back on a first draft, tearing out an
overengineered shim, or saying "no, we do not do that" when something
compiled but smelled wrong.

A large language model (Claude) typed a significant share of the
actual source. That is honest work where it lands: boilerplate,
documentation prose, mechanical refactors, the tenth variation of a
`format!(...)` line, the correct `quote!` incantation for the next
macro expansion. Mechanically competent, frequently voluminous, and —
left to itself — tasteless. Its default is to propose a speculative
JSON output mode, a trait with seven default methods nobody asked
for, or a "just in case" feature flag that nobody will ever flip.
What this crate looks like is the residue of me naming the
decisions, rejecting the drafts that missed them, and deleting the
over-engineering. The LLM is a fast typist; the mistakes and the
opinions are mine.

There is a mild irony in using a stochastic process to build a
framework whose entire point is disciplined, deterministic,
well-isolated test execution. It mostly means that "AI-assisted"
here means *supervised*: the moment supervision lapses, scope and
complexity grow back like weeds.

### What rudzio is reacting to

Honestly, frustration with Rust's async-testing story. Libtest is
synchronous by construction, so every async runtime ships its own
bespoke macro (`#[tokio::test]`, `#[compio::test]`,
`embassy_executor_macros`…) and none of them share setup or
teardown primitives. You can't easily assert that the same suite of
tests passes under tokio *and* compio *and* embassy. Output
interleaves under parallelism unless you opt out of parallelism.
Benchmarks live in their own universe (criterion, iai) with a
separate macro surface. Tests want structure — fixtures, context,
lifecycle hooks — and libtest gives you a bare `fn()`.

None of that is a complaint about the people who built the prior
art. Libtest, tokio's test harness, compio, embassy-executor —
rudzio is load-bearingly downstream of all of them. Without an
async runtime there is nothing to dispatch to; without libtest's
process protocol, `cargo test` integration is impossible. The
frustration is with the *shape* of the current ecosystem, not with
the work that got it there.

### Inspiration

The `Suite` / `Test` trait pair is an evolution of the pattern
popularised by the [`test-context`][test-context] crate — per-test
setup and teardown wrapped around a synchronous test body. Rudzio
extends that pattern into async, adds a second lifetime scope for
suite-level state, and dispatches the same body across multiple
runtimes without the user writing the dispatch logic. The debt is
direct and cheerfully acknowledged; the authors of `test-context`
found the right shape first, and this crate stands on their
shoulders.

Thanks also to the maintainers of tokio, compio, embassy, futures,
libtest, linkme, signal-hook, crossbeam, and the other crates
rudzio leans on. Naming a test framework is a vote of confidence
in their APIs staying sound.

[test-context]: https://crates.io/crates/test-context

## License

MIT or Apache-2.0, at your option.

---

In memory of Rudzisław, an orange cat (—2025-12-31). The crate is named
after him.
