# rudzio-migrate

Best-effort converter of stock cargo-style Rust tests into [rudzio]-shaped
tests. Takes a git repo whose working tree is clean, rewrites every
recognised test attribute in place, edits `Cargo.toml` for both the lib's
own `[lib] harness = false` test target and the integration
`[[test]] harness = false` binaries, appends `#[cfg(test)] #[rudzio::main]
fn main() {}` to `src/lib.rs` when there are src-resident unit tests, and
leaves a per-file backup plus an inline block-comment copy of every
converted function.

The tool does not guarantee that the generated code compiles, that tests
still pass, or that their original meaning is preserved. Expected outcome
on a non-trivial codebase: most tests compile on first pass, a short
warning list at file:line, a handful of manual fix-ups spotted via `git
diff`.

The migrator is dogfooded — `migrate/` itself is migrated and its
integration tests run through rudzio's runner.

[rudzio]: https://github.com/mykytanikitenko/rudzio

## Why this exists

Adopting rudzio on an existing crate means rewriting every `#[test]` /
`#[tokio::test]` / `#[test_context(T)]` fn, editing `Cargo.toml` for both
lib unit tests and integration tests, and appending `#[rudzio::main]`
where needed. `rudzio-migrate` does this mechanically on a clean git
tree. The result is a starting point that usually compiles with a short
warning list; review via `git diff`.

Every transformation has a golden test, warnings point at file:line with
miette, and the clean-tree + acknowledgement gates exist because the
output is not verified to compile — the tool is a diff generator, not a
guarantee.

## Install

```sh
cargo install --path migrate
```

Or via the unified `cargo-rudzio` CLI (installs both the migrator and
the single-binary test runner):

```sh
cargo install --path cargo-rudzio
cargo rudzio migrate --path /path/to/your/crate
```

Or run from a clone without installing:

```sh
cargo run -p rudzio-migrate --release -- --help
# or
cargo run -p cargo-rudzio -- migrate --help
```

Both entry points drive the same `rudzio_migrate::run::entry` function,
so behaviour and flags are identical. Use whichever fits your
install policy.

## Invocation

```
rudzio-migrate [OPTIONS]

OPTIONS:
    --path <DIR>            Repo root (default: CWD; must be inside a git repo).
    --runtime <NAME>        Default runtime for generated suites:
                            tokio-mt (default) | tokio-ct | compio |
                            futures-mt | futures-ct. Explicit per-test
                            flavors in #[tokio::test(flavor = ...)] override.
    --dry-run               Parse and report planned changes; write nothing,
                            create no backups.
    --no-shared-runner      Skip the interactive prompt that scaffolds a
                            tests/main.rs + wires its [[test]] entry.
    --no-preserve-originals Do not emit the /* pre-migration ... */ block
                            comment above each converted fn.
    --only-package <NAME>   Restrict the run to a single workspace member
                            (matched against the cargo metadata package
                            name). Other packages are left alone.
    --help, -h
```

There is no `--yes` or `--force`. The gates below are load-bearing.

## The preflight (three hard gates)

1. **Inside a git repo.** Resolved via `git rev-parse --show-toplevel`.
   If not, the tool exits `1` with a one-line explanation.

2. **Working tree is clean.** `git status --porcelain` must produce
   empty output. On failure the tool prints the exact disclaimer:

   > rudzio-migrate: refusing to run because the working tree has
   > uncommitted changes.
   >
   > This tool is not going to do any magic. It will try, on a
   > best-effort basis, to convert every test in this repository into
   > a rudzio test and — if you let it — generate a shared runner
   > entry point.
   >
   > Actions may be destructive by accident. The tool does not
   > guarantee that the generated or modified code compiles, that
   > your tests still pass, or that the conversion preserves their
   > original meaning. It is not going to save your project or make
   > your test suite magically better. Take its output as a direction
   > and eliminate most of the manual work; review every diff.
   >
   > To proceed: commit or stash your changes, then re-run.

   …then exits `1`. The clean-tree requirement is what makes
   `git diff` a reliable review surface afterwards.

3. **You type the acknowledgement phrase.** Byte-for-byte match,
   trailing `\n` or `\r\n` trimmed, everything else compared
   literally:

   ```
   I am not and idion and understand what I am doing in most cases at least
   ```

   Yes, `idion` is on purpose. The friction is the point. On a
   mismatch the tool prints `aborted: acknowledgement did not match.`
   and exits `1`.

After all three pass, a single `y`/`N` prompt asks whether to
scaffold a shared `tests/main.rs` (skipped by `--no-shared-runner`).
Then the rewriting begins.

## What gets migrated

| Input | What the tool emits | Notes |
|---|---|---|
| `#[test] fn foo()` inside `#[cfg(test)] mod ... { }` | `#[::rudzio::test] fn foo() { <body> }` — attribute replaced verbatim; signature and body kept as-is. The enclosing mod gains a `#[::rudzio::suite([...])]` attribute and its `#[cfg(test)]` is broadened to `#[cfg(any(test, rudzio_test))]` | `rudzio::test`'s codegen routes bodies through `IntoRudzioResult`, so void/explicit-unit/Result returns all work unchanged. No `_ctx: &Test` synthesis, no trailing `Ok(())` appended, no anyhow dependency forced onto users. |
| `#[tokio::test]` | as above, tokio-mt runtime | |
| `#[tokio::test(flavor = "multi_thread", worker_threads = N)]` | as above, tokio-mt runtime; `worker_threads` is dropped with a warning | |
| `#[tokio::test(flavor = "current_thread", start_paused = true)]` | as above, tokio-ct runtime; `start_paused` is dropped with a warning | |
| `#[async_std::test]`, `#[actix_rt::test]`, `#[futures_test::test]` | as above, `--runtime` default; warning about potential behaviour differences | |
| `#[compio::test]` | as above, compio runtime | |
| `#[ignore]`, `#[ignore = "reason"]`, `#[ignore("reason")]`, `#[ignore(reason = "...")]` | preserved verbatim | rudzio accepts all four forms |
| File-scope test fns in `tests/*.rs` (no wrapping `mod`) | wrapped in a synthesized `#[cfg(test)] #[rudzio::suite([...])] mod tests { use super::*; use Test; ... }` at their position; `#[::rudzio::main] fn main() {}` appended to the file if it has none | |
| `#[test_context(Ctx)] async fn foo(ctx: &mut Ctx)` with a visible `impl AsyncTestContext for Ctx` in the same crate | generates `CtxRudzioBridge<'test_context, R>` (a `Deref<Target = Ctx>` wrapper that carries the generics rudzio's macro injects) + `CtxRudzioSuite<'suite_context, R>` whose `context(...)` calls `AsyncTestContext::setup`, appended to the impl file. The suite attr now points at them. The fn sig's `&mut Ctx` is rewritten to `&mut CtxRudzioBridge` so field access still works via Deref | sync `TestContext` variant handled too |
| `#[test_context(Ctx)]` where the impl can't be located in this crate | attribute stripped, warning emitted, rest of the fn untouched; user finishes the migration by hand | |
| `#[should_panic]`, `#[should_panic(expected = "...")]` | stripped with a warning; body is not rewritten | rudzio has no panic-expectation equivalent — rewrite the body to assert the panic |
| `#[bench]` (unstable libtest) | left untouched with a warning | follow-up: auto-suggest `#[rudzio::test(benchmark = rudzio::bench::strategy::Sequential(N))]` |
| `#[rstest]` / `#[case]` / `#[values]` on a fn or its params | left untouched with a warning | rudzio has no parameterised-test equivalent |
| test fn with a `self` receiver | left untouched with a warning | rudzio tests are free fns |
| test fn with a non-`&T` / `&mut T` first param (or multiple params) | left untouched with a warning | usually an rstest case the attr detector missed |

`Cargo.toml` gets:

- `[package] autotests = false`
- `[lib] harness = false` when any `src/**/*.rs` got migrated (so the
  lib's own test target runs through `#[rudzio::main]` instead of
  libtest). Skipped on bin-only crates that have no `src/lib.rs`.
- A `rudzio = { version = "0.1", features = ["common", "<runtime-feature>"] }` entry (union of runtimes across the package's converted suites). Lands in `[dev-dependencies]`.
- One `[[test]] name = "..." path = "tests/<stem>.rs" harness = false` per `tests/*.rs` that had conversions
- If you answered `y` to the shared-runner prompt, a `[[test]] name = "main" path = "tests/main.rs" harness = false` plus a freshly-generated `tests/main.rs` with `use <crate> as _;` + `#[rudzio::main] fn main() {}`

And when any `src/**/*.rs` got migrated, the tool appends one block
at the bottom of `src/lib.rs`:

```rust
#[cfg(test)]
#[::rudzio::main]
fn main() {}
```

That's what makes `[lib] harness = false` link — the test binary
needs its own entry point. Idempotent on re-runs (parses with syn
and skips if a `fn main` already exists).

## What the tool leaves behind

For every file it overwrites, a sibling copy is created with the suffix
`.backup_before_migration_to_rudzio`:

```
src/lib.rs
src/lib.rs.backup_before_migration_to_rudzio    ← byte-identical to the pre-migration source
Cargo.toml
Cargo.toml.backup_before_migration_to_rudzio    ← byte-identical
```

Backups are never overwritten: if one already exists, the tool leaves
it alone — combined with the clean-tree gate, this means a second run
against leftover backups is already blocked by preflight. Clean them up
after you're satisfied:

```sh
find . -name '*.backup_before_migration_to_rudzio' -delete
```

Inside each converted `.rs` file, every rewritten fn carries a block
comment with the pre-migration source:

```rust
/* pre-migration (rudzio-migrate):
#[test]
fn sums_correctly() {
    assert_eq!(add(1, 2), 3);
}
*/
#[::rudzio::test]
fn sums_correctly() {
    assert_eq!(add(1, 2), 3);
}
```

Opt out with `--no-preserve-originals`.

## Warnings

The summary at the end uses [miette] to underline the exact
attribute / identifier in-source:

```
x #[should_panic] stripped; rudzio does not support panic-expectation
  ,-[src/lib.rs:9:5]
8 |     #[test]
9 |     #[should_panic]
  :     ^^^^^^^|^^^^^^^
  :            `-- here
10|     fn panics() {
  `----
```

Every warning is the tool saying "I didn't touch this, here's where
and why". There is no "did something unusual silently"; anything the
tool does without warning is something the scope table above lists as
supported.

[miette]: https://crates.io/crates/miette

## Known limits

- **Comments in mutated files are lost**, apart from the
  pre-migration block comments the tool itself injects. Line / block
  comments don't survive `syn::parse` → `prettyplease::unparse`; doc
  comments (`///`, `//!`) do, because syn represents them as
  `#[doc = "..."]` attributes. Files the tool doesn't touch stay
  byte-identical. The `.backup_before_migration_to_rudzio` copy
  preserves the original text either way.
- **Attribute order and whitespace may shift** per prettyplease's
  canonical output.
- **Multi-runtime `#[rudzio::suite([A, B, C])]` tuples** are never
  generated; the tool emits exactly one runtime per suite. Add more
  tuples by hand if you want per-test matrix coverage.
- **`rstest`** is a known blind spot. v1 detects it and refuses to
  convert; follow-up: a dedicated shape.
- **Lib `src/lib.rs` with inline-body modules** (`mod X { ... }`
  instead of `mod X;`) can't be targeted by `#[path]`, so their
  `#[cfg(test)]` suite blocks don't reach the generated
  `tests/main.rs`. Move the module body to a separate file and
  declare it as `mod X;` to make the aggregation pick it up.
- **Lib crate-root `pub use` re-exports** (e.g. `pub use
  some::helper;` in `src/lib.rs`) aren't mirrored in
  `tests/main.rs`. If a test body references `crate::helper`
  directly, the integration test binary's compilation won't find
  it — add the matching `pub use` to `tests/main.rs` by hand. Most
  test bodies use fully-qualified `crate::<mod>::...` paths and
  aren't affected.
- **Comments inside `toml_edit`-modified `Cargo.toml`** are
  preserved by `toml_edit`, but key-level indentation isn't
  necessarily matched. The rudzio dep line goes wherever
  `toml_edit` puts it.
- **`cargo fmt` is not run** on the output. Run it before committing.
- **The generated `CtxRudzioBridge` / `CtxRudzioSuite` pair is a
  starting point, not idiomatic rudzio.** When the migrator sees
  `#[test_context(Ctx)]` with an `impl AsyncTestContext for Ctx` it
  generates a `Deref<Target = Ctx>` bridge wrapper plus a ZST
  `CtxRudzioSuite` whose `setup` is a no-op and whose `context(...)`
  calls `<Ctx as AsyncTestContext>::setup()` once per test. That
  preserves semantics — every test runs a fresh setup, same as
  `test-context` did — but it also means none of the suite-level
  machinery rudzio offers (shared pool, shared server, one-time
  migration) actually gets used. The *intended* next refactor is
  to hoist the expensive parts of `AsyncTestContext::setup()` into
  a real `Suite::setup` (runs once per group) and leave the
  per-test bits in `Suite::context`. See the rudzio README's
  "Borrowing from the Suite" section for the shape. The migrator
  gets you to the point where the tests compile and run; going
  from there to "tests share a Postgres pool" is a manual step
  you'll want to do anyway.

### Lib-internal `#[cfg(test)]` tests

Two paths coexist, picked per package:

1. **Default (and simpler).** When any `src/**/*.rs` in the package
   got migrated, the tool sets `[lib] harness = false` in the
   package's `Cargo.toml` and appends `#[cfg(test)] #[rudzio::main]
   fn main() {}` to `src/lib.rs`. The lib's own test target becomes
   the rudzio runner, and `cargo test --lib` runs unit tests
   through rudzio without any extra binary. Bin-only crates (no
   `src/lib.rs`) skip both edits.

2. **Shared-runner aggregation (opt-in via the `y` prompt).** If you
   asked for the shared runner, the tool also creates
   `tests/main.rs` that `#[path]`-includes each top-level `mod X;`
   from `src/lib.rs`. Each included file is recompiled with
   `cfg(test)` active there too, so the same suite blocks register
   in both the `[lib]` test target AND the `tests/main.rs` binary.
   Good for "I want one binary that runs every flavour of test"; a
   bit wasteful otherwise.

If the tool can't find a `src/lib.rs` (bin-only crate, or a layout
where all modules are declared inline in `lib.rs`), the
shared-runner scaffold falls back to the older `use <crate> as _;`
pattern — the lib's external surface gets linked, but
`#[cfg(test)]`-gated tests inside it won't reach the aggregator.
Documented in the generated file's header.

## Recipe

```sh
# Start clean.
git status                    # must be empty output

# Try it without side-effects first.
rudzio-migrate --dry-run --path path/to/crate

# Do the conversion.
rudzio-migrate --path path/to/crate

# Review.
git diff
cargo check --tests
# Address the warnings the summary printed. Each is file:line.

# Satisfied? Drop the backups.
find . -name '*.backup_before_migration_to_rudzio' -delete

# Remove test-context if you migrated those blocks and no more
# test bodies reference its re-exported items.
cargo update  # pick up the new rudzio dep
```

If you're on a multi-package workspace and want to roll out
gradually:

```sh
rudzio-migrate --path . --only-package my-crate
```

## Safety posture, in one paragraph

The tool assumes you're a grown-up with a git client. It refuses to
touch anything on a dirty tree, asks you to type a phrase before it
starts, keeps a byte-identical copy of every file next to the
original, and preserves the pre-migration source of every fn it
rewrites as a block comment one line above the new version. All of
that exists so that after it runs, `git diff` is trustworthy and
`git checkout -- <file>` plus `rm *.backup_*` gets you back to the
starting state. The rest — compiles, tests pass, tests still mean
what they meant — is on you.
