# rudzio migration bug register

Self-contained register of the cargo-rudzio / rudzio 0.1.2 migration bugs, toolchain 1.99.1. Ordered by severity.

## #8 (CRITICAL) — bin-only crates never aggregate integration tests

`cargo rudzio test -p <bin-crate>` panics at startup.

Minimal repro — crate with only src/main.rs (no [lib]) + tests/x.rs:

```rust
// src/main.rs
fn main() {}
```

```rust
// tests/x.rs
#[rudzio::suite([(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test)])]
mod tests {
    #[rudzio::test]
    async fn t(_c: &rudzio::common::context::Test) -> anyhow::Result<()> { Ok(()) }
}
```

```toml
# Cargo.toml
[package]
name = "mybin"
version = "0.1.0"
edition = "2024"
autotests = false

[[test]]
name = "main"
path = "tests/main.rs"
harness = false

[dev-dependencies]
rudzio = { version = "=0.1.2", features = ["runtime-futures", "common"] }
```

```
cargo rudzio test -p mybin
# thread 'main' panicked at linkme-0.3.36/src/distributed_slice.rs:246:18:
# unsafe precondition(s) violated: slice::from_raw_parts requires the pointer to be aligned and non-null
```

Root cause (`cargo-rudzio/src/generate.rs` ~:4141-4160): for a member whose `src_lib_path` is None, `build_bridge_lib_rs` returns a stub lib.rs with zero `#[distributed_slice]` statics, hardcoded with a comment "correct, because bin-only members have no test surface to register". The `tests/*.rs` files are only mounted in the lib-bearing branch. Result: TEST_TOKENS slice is empty → from_raw_parts(NULL, 0).

## #6 (CRITICAL) — workspace-wide runner duplicates path-dep crates

Blocks the single shared runner across crates.

Minimal repro — workspace where member a path-depends on member b, both depend on rudzio:

```toml
# root Cargo.toml
[workspace]
members = ["crates/*"]
```

```toml
# crates/a/Cargo.toml
[dependencies]
b = { path = "../b" }

[dev-dependencies]
rudzio = { version = "=0.1.2", features = ["runtime-futures", "common"] }
```

```
cargo rudzio test        # NO -p
# error[E0308] + E0271/E0277/E0599 — 107 errors
# note: multiple different versions of crate `b` in the dependency graph
#   - crates/b/src/lib.rs (expected)
#   - members/b/src/lib.rs (found)
```

Root cause: aggregator mounts each member's path deps into both the original `crates/` and a generated `members/`, so rustc sees two distinct crates for the same source. `cargo rudzio test -p a` (single crate) works fine.

## #7 (HIGH) — extern crate self as X collides with mod X;

Minimal repro — crate named project whose lib.rs has a self-named module:

```rust
// crates/project/src/lib.rs
mod project;
```

```
cargo rudzio test -p project
# error[E0260]: the name `project` is defined multiple times
#   --> members/project/src/lib.rs:159:1
#  14 | mod project;                         (previous definition)
# 159 | #[cfg(rudzio_test)] extern crate self as project;
```

Root cause: the auto-runner copies lib.rs and appends `#[cfg(rudzio_test)] extern crate self as project;`, which collides with the crate's own `mod project;`.

## #2 (HIGH) — [lib] harness = false breaks clippy --all-targets

Minimal repro — any crate with src tests; add the README-documented `[lib] harness = false`:

```toml
[lib]
harness = false
```

```
cargo clippy --all-targets --all-features -- -D warnings
# error[E0601]: main function not found in crate X
```

Root cause: `harness = false` turns the lib test target into a binary with no main. Conflicts with clippy `--all-targets`.

## #5 (HIGH) — #[rudzio::suite] can't register macro_rules! tests

Minimal repro:

```rust
macro_rules! make_test {
    ($n:ident) => {
        #[rudzio::test] async fn $n(_c: &rudzio::common::context::Test) -> anyhow::Result<()> { Ok(()) }
    };
}

#[rudzio::suite([(runtime = rudzio::runtime::futures::ThreadPool::new, suite = rudzio::common::context::Suite, test = rudzio::common::context::Test)])]
mod tests {
    make_test!(a);
}
```

```
# error[E0107]: missing generics for struct `rudzio::common::context::Test`
```

Root cause: `apply_runtime_generics` only rewrites direct ItemFns; macro bodies keep bare `&Test`. Even if they compiled, the suite scan runs pre-expansion so they'd be unregistered.

## #4 (MEDIUM) — panic = "abort" not propagated to the runner

Minimal repro — root Cargo.toml sets `[profile.dev] panic = "abort"`, one test body has `panic!()`:

```
cargo rudzio test
# test FAILED (panicked), exit 1   ← NOT SIGABRT/134
```

Root cause: the auto-runner is a separate generated project; it doesn't inherit the workspace `[profile]` (so `panic = "abort"` never applies).

## #3 (MEDIUM) — coverage broken through cargo rudzio

Minimal repro:

```
eval "$(cargo llvm-cov show-env --sh)"
cargo rudzio test
cargo llvm-cov report
```

Root cause (two halves):

1. `cargo llvm-cov show-env` **without** `--sh` emits a single line of
   `NAME='value'` assignments (no `export ` prefix). `eval`-ing that in
   bash/zsh creates shell-local variables that child processes do not
   inherit, so the inner cargo build is uninstrumented and writes zero
   `.profraw`. The `--sh` flag emits `export NAME='value'` lines, which
   `eval` propagates. `cargo rudzio` fails loudly when `CARGO_LLVM_COV`
   is set but `RUSTC_WRAPPER` / `LLVM_PROFILE_FILE` are missing.
2. The aggregator otherwise builds into
   `<target>/rudzio-auto-runner-<hash>/target`; under coverage the runner
   and its `.profraw` are anchored at `CARGO_LLVM_COV_TARGET_DIR` so
   `cargo llvm-cov report` can collect them.

## #1 (LOW) — cargo-rudzio's own self-test suite won't compile on 1.99.1

Minimal repro:

```
git clone https://github.com/mykytanikitenko/rudzio && cd rudzio
cargo test -p cargo-rudzio   # 99 errors: E0432 unresolved imports, E0599 method-not-found on `!`, phased-out rudzio::suite type-annotation warning
```

## #9 (LOW, docs) — README documents nonexistent --format json

Only `--format <pretty|terse>` exists; `cargo rudzio test --format json` is a stale README example.
