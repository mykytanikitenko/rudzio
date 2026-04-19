# rudzio

Async test framework for Rust with pluggable runtimes and per-test
`setup`/`teardown`. Each test runs against a fresh test context that you build
on top of a shared per-suite global. Cancellation, per-test and per-run
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
use common_context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
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
rudzio = { git = "https://github.com/mykytanikitenko/rudzio", features = ["runtime-tokio"] }
# Optional: a ready-to-use Global/Test pair on top of CancellationToken + TaskTracker.
common-context = { git = "https://github.com/mykytanikitenko/rudzio" }
```

`harness = false` is required — the `#[rudzio::main]` attribute installs the
runner that walks every `#[rudzio::test]` registered via `linkme`. Not yet on
crates.io; pin to a commit in your `Cargo.toml` if you care about
reproducibility.

## Concepts

Three traits, three lifetimes, in strict outer-to-inner order:

```
'runtime  >  'context_global  >  'test_context
```

| Trait                              | Lives for         | Created                            | Dropped                          |
|------------------------------------|-------------------|------------------------------------|----------------------------------|
| `Runtime<'rt>`                     | `'runtime`        | once per `(runtime, global)` group | when the group thread exits      |
| `Global<'context_global, R>`       | `'context_global` | once per group, after `Runtime`    | after the last test in the group |
| `Test<'test_context, R>`           | `'test_context`   | once per test, in `Global::context`| after the test body returns      |

`Self::Test` on `Global` is a GAT — `Self::Test<'test_context>` — so the
per-test context value genuinely lives in the per-test borrow lifetime, not
in the global's. That's what makes `&mut TestCtx` parameters compile.

`#[rudzio::test]` accepts either `&Ctx` or `&mut Ctx` as the first argument,
sync or async body, returning `anyhow::Result<()>` (or any
`Display`-able error wrapped in `Result`).

## Multiple runtimes per test

Each tuple in the `#[rudzio::suite]` list is a separate `(runtime, global)`
configuration. The same test bodies run against each of them:

```rust
#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
    (
        runtime = CompioRuntime::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    #[rudzio::test]
    async fn runs_on_every_runtime(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}
```

The runner spawns one OS thread per `(runtime, global)` pair. Multiple
`#[rudzio::suite]` blocks declaring the same `(runtime, global)` collapse into
one thread / one runtime / one global instance — keyed by a compile-time hash
of the `(runtime_path, global_path)` token strings.

## Runtimes

Behind feature flags. Default: none — pick what you need.

- `runtime-tokio` → `rudzio::runtime::tokio::{Multithread, CurrentThread, Local}`
- `runtime-compio` → `rudzio::runtime::compio::Runtime`
- `runtime-embassy` → `rudzio::runtime::embassy::Runtime`

Implementing your own `Runtime<'rt>` is a regular trait impl; nothing in the
runner is hard-coded to a specific runtime crate.

## Custom contexts

`common-context` ships a ready-to-use `(Global, Test)` pair on top of
`tokio_util::sync::CancellationToken` + `tokio_util::task::TaskTracker`. If you
need your own (a `sqlx::PgPool`, an HTTP server handle, a mock clock), define
structs that implement `rudzio::context::Global` and `rudzio::context::Test`.
See `e2e/src/bin/custom_context_tokio_mt.rs` for a minimal hand-rolled
example.

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

Each suite group gets a child of the run-wide root token, so a `Global::teardown`
that cancels its stored token only fans out within its own group.

## CLI flags (libtest-compatible subset)

```
<filter>                    positional substring match against test name
--skip <s> / --skip=<s>     exclude tests whose name contains <s>
--ignored                   only run #[ignore]d tests
--include-ignored           run every test, ignored or not
--list                      list test names and exit
--test-threads=N            in-flight test concurrency per group
--format=pretty|terse       output style
--color=auto|always|never   colour control
--test-timeout=N            per-test timeout (seconds)
--run-timeout=N             whole-run timeout (seconds)
```

`RUST_TEST_THREADS=N` and `NO_COLOR=1` are honoured.

## Status

`0.1.x`. The shape of `Global`, `Test`, `Runtime` and the suite macro is
intentionally kept stable — there are tests asserting on the rendered output
format and on cancellation/teardown behaviour. Internals (`SuiteRunner`,
`TestToken` layout, `RuntimeGroupKey` hashing) are `#[doc(hidden)]` and may
change.

## License

MIT or Apache-2.0, at your option.

---

In memory of Rudzisław, an orange cat (—2025-12-31). The crate is named
after him.
