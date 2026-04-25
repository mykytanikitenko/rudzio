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
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", features = ["runtime-tokio", "common"] }
```

Features are all off by default. Pick what you need:
- `common` — ready-made `Suite`/`Test` pair on top of `CancellationToken` +
  `TaskTracker` at `rudzio::common::context`. Omit if you're writing your own
  context types.
- `runtime-tokio` — `rudzio::runtime::tokio::{Multithread, CurrentThread, Local}`.
- `runtime-compio` — `rudzio::runtime::compio::Runtime`.
- `runtime-embassy` — `rudzio::runtime::embassy::Runtime`.
- `runtime-futures` — `rudzio::runtime::futures::ThreadPool` (on top of
  `futures::executor::ThreadPool`).

`harness = false` is required — the `#[rudzio::main]` attribute installs the
runner that walks every `#[rudzio::test]` registered via `linkme`. Not yet on
crates.io; pin to a commit in your `Cargo.toml` if you care about
reproducibility.

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

- `runtime-tokio` → `rudzio::runtime::tokio::{Multithread, CurrentThread, Local}`
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

## Workspace-wide single-binary test runner

Tests live in `tests/*.rs` (per-crate integration tests), so
`cargo test -p <crate>` works the way you'd expect. If you also want a single
binary that runs every crate's tests in one process — one runtime, one
scheduler, one pass of output — add a `test-runner` crate that pulls each
sibling's test file in via `#[path]`:

`test-runner/Cargo.toml`:
```toml
[package]
name = "test-runner"
edition = "2024"

# Own (single-crate) workspace so this crate's feature selections do NOT
# unify into the parent workspace's other binaries.
[workspace]

[dependencies]
rudzio = { path = "..", features = ["runtime-tokio", "common"] }
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

### Aggregating tests that spawn `[[bin]]` targets

Integration tests that use `env!("CARGO_BIN_EXE_<name>")` to spawn their
crate's `[[bin]]` targets — e.g. tests that drive child processes to check
stdout — don't compile when you `#[path]`-include them from outside their
defining crate: Cargo only populates `CARGO_BIN_EXE_<name>` for integration
tests of the crate that declares the `[[bin]]`.

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
bin. Your `#[path]`-included integration test then compiles with
`env!("CARGO_BIN_EXE_<name>")` working exactly the way it did in the bin
crate's own tests.

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
  binaries reachable via `env!`.

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
