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

Three runnable examples in `examples/` cover the common shapes:

- `cargo run --example basic` — one runtime, the `common` context, a
  trivial suite (pass / yield / `#[ignore]`).
- `cargo run --example multi_runtime` — the same test bodies under
  tokio's Multithread + CurrentThread + compio, all in one
  `#[rudzio::suite]` block.
- `cargo run --example custom_context` — hand-rolled `Suite` / `Test`
  impls with shared suite-level state.

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

`#[rudzio::test]` accepts either `&Ctx` or `&mut Ctx` as the first argument,
sync or async body, returning `anyhow::Result<()>` (or any
`Display`-able error wrapped in `Result`).

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

## Status

`0.1.x`. The shape of `Suite`, `Test`, `Runtime` and the suite macro is
intentionally kept stable — there are tests asserting on the rendered output
format and on cancellation/teardown behaviour. Internals (`SuiteRunner`,
`TestToken` layout, `RuntimeGroupKey` hashing) are `#[doc(hidden)]` and may
change.

## License

MIT or Apache-2.0, at your option.

---

In memory of Rudzisław, an orange cat (—2025-12-31). The crate is named
after him.
