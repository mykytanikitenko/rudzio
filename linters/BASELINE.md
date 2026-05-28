# Linter Baseline — `non-result-fns`

**Captured:** 2026-05-27 at commit `fa80fb2ddb721b18e753b147afcf84e69adcb5e2`
**Linter:** `linters/non-result-fns/`
**Gate state:** HARD (exit 1 if any hazard op detected)
**Goal:** zero hazard ops, zero non-Result fns whose body silences a real failure path

## Why this baseline exists

The linter `linters/non-result-fns/` was introduced after Bug #16
surfaced a silent coverage gap (rudzio surface inside `[[bin]]`
source files silently invisible). The post-mortem identified the
root defect as **information loss at function boundaries**: a fn
returning `bool`, `Vec`, `Option`, `()`, or any non-`Result` type
cannot propagate a failure to its caller. Combined with workspace
clippy lints that block obvious panic paths (`arithmetic_side_effects`,
`indexing_slicing`, etc.), the developer reflex moves to escape
hatches:

- `unwrap_or` family — swallow failure into a default
- `saturating_*` / `wrapping_*` — mask overflow as a clamp/wrap

Both are silent. The Bug #16 family slipped past every existing
gate this way. The linter codifies the inverse contract: a fn that
cannot return an error must not contain operations that can fail.

This baseline is the snapshot of the codebase at the moment the
gate was wired in. Every fix from here on monotonically reduces
the numbers. The CI gate keeps them from drifting back up.

## Headline numbers

```
Total non-Result fns enumerated:  576
  with hazard ops in body:        108  (18.7%)
  with no hazard ops in body:     468
Total hazard ops:                 318
```

## Hazard ops by kind

| Kind | Count | What it is |
|---|---:|---|
| `unwrap()` | 0 | Lint-blocked workspace-wide. |
| `expect(...)` | 0 | Lint-blocked workspace-wide. |
| indexing `expr[expr]` | 0 | `indexing_slicing = forbid`. |
| arithmetic op | 3 | `arithmetic_side_effects = forbid` — these survive via `#[expect(...)]`. |
| panic-class macro | 1 | `panic!`, `unreachable!`, `todo!`, `unimplemented!`, `assert!`, `debug_assert!`. |
| numeric `as` cast | 0 | Lint-blocked. |
| **silencing combinator** | **156** | `unwrap_or`, `unwrap_or_default`, `unwrap_or_else`, `.ok()`, `.err()`, `ok_or`, `ok_or_else` — swallows failure into a default. |
| **masking arithmetic** | **158** | `saturating_*`, `wrapping_*` — clamps/wraps overflow silently vs `checked_*` which surfaces it. |

The two leading hazard classes are roughly equal in volume and were
previously invisible because they are the legitimate escape hatches
the existing clippy lints push developers toward. They are the
post-mortem's "hidden enemies".

## By return type (fn counts)

| Return type | Count |
|---|---:|
| Custom type | 152 |
| `()` | 112 |
| Other | 100 |
| `String` / `&str` | 68 |
| `bool` | 57 |
| `Option<T>` | 45 |
| `Vec<T>` | 16 |
| Numeric primitive | 14 |
| `PathBuf` / `&Path` | 8 |
| `Map` / `Set` | 4 |

## By crate

| Crate | Non-Result fns | Hazard ops |
|---|---:|---:|
| `src/` (rudzio) | 314 | 234 |
| `migrate/` | 127 | 57 |
| `cargo-rudzio/` | 94 | 23 |
| `macro-internals/` | 25 | 1 |
| `cargo-rudzio-test-helpers/` | 12 | 0 |
| `macro/` | 4 | 3 |

## Top 10 hot-spot files

| Hazards | File |
|---:|---|
| 71 | `src/output/render.rs` |
| 61 | `src/bench/report.rs` |
| 27 | `migrate/src/emit.rs` |
| 25 | `src/bench/progress_snapshot.rs` |
| 25 | `src/runner.rs` |
| 21 | `cargo-rudzio/src/generate.rs` |
| 21 | `migrate/src/rewrite.rs` |
| 14 | `src/config.rs` |
| 10 | `src/shuffle.rs` |
| 9 | `src/common/time.rs` |

Bench reporting + output rendering dominate the top of the list —
both are statistics-/formatting-heavy modules that reach for
`saturating_*` to dodge overflow lints on aggregate computations.

## Top 15 individual timebombs

| Hazards | Site | Signature |
|---:|---|---|
| 25 | `src/bench/progress_snapshot.rs:66` | `fn from_samples() -> Self` |
| 21 | `src/bench/report.rs:81` | `fn ascii_histogram() -> String` |
| 14 | `src/output/render.rs:1483` | `fn bench_histogram_lines() -> Vec<String>` |
| 10 | `src/runner.rs:723` | `fn merge() -> Self` |
| 9 | `src/common/time.rs:22` | `fn fmt_duration() -> String` |
| 9 | `src/suite.rs:407` | `fn merge() -> Self` |
| 8 | `src/output/render.rs:981` | `fn record_outcome() -> ()` |
| 8 | `cargo-rudzio/src/generate.rs:1150` | `fn collect_package_metadata_fields() -> PackageMetadataFields` |
| 7 | `src/bench/report.rs:400` | `fn percentile_permille() -> Option<Duration>` |
| 7 | `src/bench/report.rs:436` | `fn std_dev() -> Option<Duration>` |
| 7 | `src/output/render.rs:1379` | `fn running_line() -> String` |
| 7 | `src/output/render.rs:1561` | `fn lifecycle_line() -> String` |
| 6 | `src/bench/report.rs:165` | `fn detailed_summary() -> String` |
| 6 | `src/shuffle.rs:13` | `fn next_splitmix64() -> u64` |
| 6 | `migrate/src/emit.rs:409` | `fn splice_bridge_before_first_suite_or_main() -> String` |

## Resolution policy (every hazard, no exceptions)

For every hazard the linter reports, exactly one of these must
happen:

1. **Refactor the containing fn to return `Result`** and propagate
   the underlying error. Replace `saturating_*` with `checked_*`
   and `?`-propagate the `None` arm. Replace `unwrap_or_default` /
   `.ok()` with explicit `?` on a `Result`. Replace `unwrap()` /
   panic macros with explicit error returns.
2. **Annotate with `#[expect(...)]` and a written justification.**
   Same trade-off as the existing `arithmetic_side_effects` /
   `indexing_slicing` lints — local, documented, reviewable. The
   bar is a comment that names the invariant guaranteeing safety.
   `#[allow]` is forbidden workspace-wide; `#[expect]` only.

There is no third option. Silent `saturating_*` / `unwrap_or` calls
are now CI failures.

## How to re-run

```sh
just check-lints           # standalone run
just ci                    # check-lints is now part of the ci aggregate
```

Detailed sorted report regenerates at `linters/non-result-fns-report.txt`
on every invocation. The report is grouped by hazard total
descending so the highest-leverage fixes surface first.
