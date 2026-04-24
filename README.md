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
and `rudzio::context::Test`. See `e2e/src/bin/custom_context_tokio_mt.rs` for a
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

Two things to watch:

- **Exclude from parent workspace.** Put `exclude = ["test-runner"]` in the
  parent's `[workspace]` (or give `test-runner` its own `[workspace]` block).
  Otherwise Cargo's workspace-wide feature unification propagates whatever
  features the aggregator requests back into every sibling crate that links
  those deps — not what you want.
- **Tests that reference `env!("CARGO_BIN_EXE_<name>")`** (e.g. integration
  tests that spawn their crate's `[[bin]]` targets) can't be aggregated this
  way: those env vars are only set when Cargo builds that crate's own
  integration-test binary. Keep those tests per-crate.

Rudzio's own workspace demonstrates both modes side-by-side: `cargo test
--workspace` runs everything per-crate; `(cd test-runner && cargo run)`
aggregates rudzio's multi-runtime dogfood suite and `rudzio-macro-internals`'
parser tests into one 113-test binary.

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
`Suite::setup`. Runtimes can read what they need. Today:

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
