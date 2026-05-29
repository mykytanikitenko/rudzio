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

---

## P-4 extension snapshot

**Captured:** 2026-05-28 immediately after PLAN.md Row 16 lands.
**Commit:** at HEAD on this branch (P-4 commit).
**What changed in the linter:** trait-impl skip list (Display, Debug,
Iterator, etc. — see `TRAIT_SKIP_LIST` in `linters/non-result-fns/src/main.rs`),
allowlist file at `linters/non-result-fns/allowlist.toml`, new
`TODO(rs-cleanup)` substring counter, new `#[expect(...)]` AST counter,
new CLI flags `--gate-todos` and `--gate-expects N` (both default
inactive), qualified-path tracking (`<crate>::<mod>::<Type?>::<fn>`)
for stable allowlist addressing.

### Headline numbers (post-P-4)

```
Total non-Result fns enumerated:  589
  with hazard ops in body:        107  (18%)
  with no hazard ops in body:     482
Allowlisted fns (excluded):       0    (allowlist.toml empty)
Total hazard ops:                 318
TODO(rs-cleanup) markers:         0
#[expect(...)] attrs (scan dirs): 10
```

### Drift from original baseline (`fa80fb2`)

| Metric | Baseline | Post-P-4 | Δ | Note |
|---|---:|---:|---:|---|
| Total non-Result fns | 576 | 589 | +13 | New code in workspace since baseline; trait-skip removed 5 std-impl fns so the raw growth is +18. |
| Hazard ops | 318 | 318 | 0 | Unchanged headline; mid-flight cleanup (`split_lines` retype removed 1 silencing) was balanced by 1 new masking-arith elsewhere. |
| Silencing combo | 156 | 155 | −1 | `split_lines` retype dropped its `unwrap_or("")` (P-3 / row 15). |
| Masking arithmetic | 158 | 159 | +1 | One new `saturating_*` site appeared in the interval; not tracked. |
| `#[expect(...)]` (scan dirs) | — | 10 | new | Counter added by P-4. Cross-check: `expect-allow-ban` linter scans the full repo and counts 155 (P-0). The 10 here are the subset under `src/`/`cargo-rudzio/src/`/etc. |
| `TODO(rs-cleanup)` | — | 0 | new | Counter added by P-4. Phase 2 sweep is the planned activator of `--gate-todos`. |

### Gate state matrix

| Gate | Active? | Threshold | What flips it |
|---|---|---|---|
| `hazard ops > 0` | YES | 0 | Always-on. Same as baseline. |
| `--gate-todos` | NO (opt-in) | TODO count > 0 | Activated when Phase 2 marker sweep starts. |
| `--gate-expects N` | NO (opt-in) | `#[expect]` count > N | Activated phase-by-phase during M-2 to drive the count monotonically down. |

`--gate-expects` is designed so the campaign sets `N = current_count`
on activation, then ratchets `N` downward as fns convert to
`Result<T>`. Same monotonic-decrease discipline as the `non-result-fns`
hazard gate.

### Allowlist policy

`linters/non-result-fns/allowlist.toml` ships empty. Adding an entry
is a deliberate human action with a reviewed `reason` string. The
intent is `M-2` use only — fns whose signature genuinely cannot
return `Result<T>` (locked by std trait, etc.). All other hazards
get refactored, not allowlisted.
