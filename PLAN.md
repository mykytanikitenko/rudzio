# PLAN.md — Bug #17 + Bug #18 fixes, then Result hazard cleanup

**Created:** 2026-05-28 (revised after 5-agent gap audit)
**Branch:** `main`
**Most recent commit:** `5c7b359` ("refactor(tests): Phase B Tier-3 migrations — bug_19 + synth_name_leak + test_filter")
**Status:** Phase 0 complete (`820d257`+`1ebe772`). Phase B framework landed (`92151ff`) — `IsolatedCargoSuite`/`IsolatedCargoTest` now own per-test isolation via warmup snapshot + full-recursive copy. Phase B migrations complete: 8 of 10 specs using `IsolatedCargoSuite` now use `ctx.consumer_workspace()`; remaining 2 (`cross_bridge`, `bug_14`) intentionally not migrated (parametric fixtures). Row 12 (aggregator pre-compile in snapshot, `--remap-path-prefix`) PENDING. P-0 (`#[expect]`/`#[allow]` per-site review) is the natural next item. See Campaign tracking matrix below for live row-level state. Future-PP updates the matrix as work lands; commit hashes filled before push.

## Revision history

- **v1** (this morning): initial plan covering Bug #17 + Bug #18 fixes + full Result cleanup. Locked via AskUserQuestion ratification.
- **v2** (earlier this session): rewrite after 5 parallel gap-audit agents found 5 Critical + 12 Should-fix + 5 Operational gaps. Key changes:
  - Bug #18 fix switched from sentinel-wipe (option d, has concurrency race) to subdir-hash (option c, race-free).
  - `build.rs` drops broken `cargo:rerun-if-changed=build.rs` (was defeating its own purpose).
  - P-2 (manual Clone audit) deleted — zero manual `impl Clone for T` in workspace.
  - P-3 (Iterator retype) scope corrected — zero `impl Iterator for T`; only two return positions.
  - Phase 2's `#[expect]` escape hatch replaced with allowlist file (custom linter can't be `#[expect]`-suppressed).
  - **P-0 NEW**: audit existing `#[expect]` attributes — they escaped code review and are red flags themselves.
  - Operational fixes for worktree target dir, cargo-env.sh isolation, 120s shell timeout, tool binary paths.
- **v3** (this session, mid-execution): adds Phase 0c (`expect-allow-ban` linter) + Campaign tracking matrix. Key user directives:
  - `#[expect]` AND `#[allow]` both detected uniformly; user intuition: existing reason strings may be LLM escape hatches; no auto-baseline.
  - Linter ships with EMPTY allowlist; CI goes RED on `check-lints` — this is a FEATURE.
  - Failing linter MUST NOT block other CI checks (fmt, clippy, audit, deny, tests); Justfile `ci` aggregate splits, CI workflow adds parallel `lints` job.
  - `unsafe_code` consolidation deferred to a future crate (out of scope); linter does not special-case unsafe_code.
  - P-0 (originally "audit expects") restructured: Phase 0c builds the enumeration tool; P-0 becomes the per-site disposition follow-up.

## Campaign tracking matrix

Live progress log. Status values: `DONE` (with commit hash) | `IN PROGRESS` | `PENDING` | `BLOCKED` | `N/A`. Update whenever a row's state changes; commit updates with corresponding code changes.

| # | Item | Status | Commit |
|---|---|---|---|
| 0 | TypeId-based `RuntimeGroupKey` refactor (UB fix at dispatch) | DONE | `fa80fb2` |
| 1 | Bug #16 reproducer + 6 surface-visibility tests + `non-result-fns` linter + `BASELINE.md` snapshot | DONE | `ce2f5d8` |
| 2 | Step 2.5: integrity check `validate_no_unmounted_markers` + `silent_member_drop_invariant_spec` | DONE | `f20b143` |
| 3 | Step 3: Bug #16 structural fix — mount bin sources via `collect_unmounted_bin_entries` | DONE | `261fa8a` |
| 4 | Step B: `discover_test_root_helpers` lifts entry-file gate; flat `tests/<name>.rs` resolves submods under `tests/<stem>/` | DONE | `d08826d` |
| 5 | Phase 0a: Bug #17 fix (`is_directory_style_bin` heuristic) + Tests 2/3/4 in `bug_16_bin_source_tests_invisible_spec` | DONE | `820d257` |
| 6 | Phase 0b: Bug #18 fix (`build.rs` + `BUILD_STAMP` + `aggregator_subdir_name`) + `bug_18_aggregator_cache_invalidation_spec` | DONE | `820d257` |
| 7 | Phase 0c: `expect-allow-ban` linter crate + Justfile `check-lints` isolation + CI workflow parallel `lints` job | DONE | `820d257` |
| 8 | Phase 0 single batched commit (covers 0a + 0b + 0c) + push to `origin/main` | DONE | `820d257` |
| 9 | Bug #19 fix: per-bin bridge crate preserves `crate::*` root in mounted bin sources + `bug_19_bin_crate_root_resolution_spec` (6 proactive tests) | DONE | `1ebe772` |
| 10 | Phase B framework: `IsolatedCargoSuite` warmup snapshot (template consumer workspace) + `IsolatedCargoTest` per-test full-recursive-copy + RAII cleanup; one spec migrated as proof-of-concept (`bug_18`). Aggregator pre-compile in snapshot DEFERRED (cargo's incremental cache uses absolute paths; cross-path reuse needs `--remap-path-prefix` work). | DONE | `92151ff` |
| 11 | Phase B migration: 8 of 10 specs using `IsolatedCargoSuite` migrated to `ctx.consumer_workspace()` (`bug_18`/`silent_member_drop`/`parallel_invocations`/`surface_visibility`/`bug_16`/`bug_19`/`synth_name_leak`/`test_filter`). `cross_bridge` + `bug_14` intentionally NOT migrated (parametric fixture shapes incompatible with template — see `.opencode/issues.md` 2026-05-28 Tier-4 entry). | DONE | `6645ab6`+`a79ccae`+`ff4d66d`+`5c7b359` |
| 12 | Phase B aggregator-pre-compile-in-snapshot follow-up. DEFERRED after design analysis (see `.opencode/issues.md` 2026-05-28 Row 12 entry): cargo-rudzio regenerates the aggregator every invocation, per-test consumer paths differ from template path (cargo fingerprints + bridge `[lib].path` are absolute), and the dominant warmup cost (cargo registry) is already mitigated by `prepopulate_cargo_home`. Alternative path (`cargo install` rudzio as a pre-built artefact, skip per-test recompile) flagged for future. | DEFERRED | N/A |
| 13 | P-0 cleanup: per-site review of `#[expect]`/`#[allow]`. All 155 sites classified with fresh human-authored reasons (53 ALLOWLIST legitimate patterns + 102 DEFER awaiting structural fixes — unsafe-code consolidation crate D-EXP.5, env-guard helper in `cargo-rudzio-test-helpers`, `rudzio::test` macro contract relaxation, Phase B follow-up). `expect-allow-ban` gate now GREEN (155 hits, 0 unallowlisted, 0 orphans). | DONE | `14934fe` |
| 14 | P-1 cleanup: `Default` impl removal completed in 8 batches (1 manual `impl Default for WorkspaceDepSpec` removed + 22 `#[derive(Default)]` dropped + 2 struct-update consumers updated + 9 `T::default()` call-sites refactored). New `linters/no-default-impls/` syn-based binary added + wired into `just check-lints` so future Default impls fail CI. Verification: linter reports 0 hits; `cargo clippy --workspace --all-targets -- -D warnings` clean. | DONE | `cfedaf5`+`ca7e417`+`2d770e2`+`014f350`+`cfc28fa`+`b791b4b`+`5c2d2d8`+`1f60be7` |
| 15 | P-3 cleanup: `Iterator` retype. Wider audit corrects v2: workspace has ONE return-position `impl Iterator` site, not two. PLAN.md v2 audit conflated `cargo-rudzio/src/generate.rs:2686` (parameter position `entries: impl Iterator<...>`) with a return position; only `src/output/render.rs:1272` `split_lines` is a return position. Decision: retyped Item `&str` → `Result<&str, anyhow::Error>` per D-2 v2 strict reading — silent `unwrap_or("")` swallowed real test stdout/stderr on non-UTF-8 input. Three call-sites (`emit_captured_block` ×2 + `emit_plain_lines`) updated to format `<{err:#}>` diagnostic on Err. Added `anyhow` to rudzio `[dependencies]` (was dev-only). Proactive test `captured_block_surfaces_non_utf8_diagnostic` in `tests/render_idle_redraw.rs` (45 tests pass across 9 runtimes). Workspace clippy clean. | DONE | `7f6eea9` |
| 16 | P-4 cleanup: `non-result-fns` linter extension. Added (a) trait-impl skip list (`TRAIT_SKIP_LIST` const: std::fmt/cmp/hash/ops/marker/convert/iter/io + async traits — 70+ entries; skip whole impl when trait's last segment matches), (b) qualified-path tracking (`<crate>::<mod>::<Type?>::<fn>` via threaded `mod_path: Vec<String>`), (c) `allowlist.toml` parser at `linters/non-result-fns/allowlist.toml` with `[[allow]] path/reason` schema; allowlisted findings still appear in report but excluded from every aggregate + gate, (d) `TODO(rs-cleanup)` substring counter, (e) `#[expect(...)]` AST attr counter via `ExpectVisitor`, (f) CLI flags `--gate-todos` (fail when TODO > 0) and `--gate-expects N` (fail when expect count > N) — both default INACTIVE per "initially the new gates are inactive; activated phase-by-phase". Verification: extension compiles clean (`cargo clippy --no-deps -- -D warnings`); reports `total=589 fns / 318 hazards / 0 allowlisted / 0 TODOs / 10 expects`; trait-skip drops 5 fns from prior 594 (std Drop impls etc.); synthetic allowlist entry round-trip verified (entry added → count incremented + hazards excluded → entry removed → counts restored); `--gate-expects 5` exits non-zero with FAIL message; `--gate-expects 1000` does not. `linters/BASELINE.md` extended with P-4 snapshot + drift-from-baseline table + gate-state matrix. | DONE | `3f7026a` |
| 17 | M-1: `rs-result-sig-rewrite` syn-based signature transformer tool. New crate `linters/rs-result-sig-rewrite/` (added to linter sub-workspace `members`). Hybrid syn+text strategy: parse with `syn::parse_file`, collect byte-range `Edit`s via `proc_macro2::Span::byte_range()` (`span-locations` feature), apply descending so earlier edits don't shift later offsets; comments + formatting between edits preserved verbatim. CLI: `--file <abs-path> [--dry-run] [--write --confirm-write]`. Exit codes 0/1/2/3 per spec. Edge cases handled per PLAN.md table (lines 833-850) — 16 synthetic snapshot tests one-per-edge-case (trait_impl_skip, iterator_next_skip, async_fn, const_fn_skip, generic_where_clause, impl_trait, never_return_skip, existing_question_mark, macro_tail, macro_rules_skip, unit_return, return_value, return_no_value, tail_side_effects, hazard_todos, intra_file_callsite). Verification: `cargo build -p rs-result-sig-rewrite` clean; `cargo clippy --all-targets --no-deps -- -D warnings` clean (after fixup `97c7303` removed `&ret` needless-borrow + unified `run_case` into single fn so per-test-binary dead-code resolves); 16/16 tests pass; dry-run against `src/common/time.rs` produces correct diff (`fmt_duration -> String` → `-> anyhow::Result<String>`, tail wrapped in `Ok(...)`, 9 `TODO(rs-cleanup)` markers above each `unwrap_or`). | DONE | `79754b0`+`97c7303` |
| 18 | M-2: per-module non-Result → Result conversion. IN PROGRESS. Bulk M-1 transform of all 85 scan-dir files attempted + preserved on branch `wip/m2-bulk-transform` (`5d4ab70`, NOT compiling — 496 errors, dominated by workspace-internal trait-contract walls: M-1 transforms trait *impl* methods but not the trait *declarations* — `Runtime`/`ModeReporter`/`Strategy`/context traits — and runtime `block_on`/`spawn` returning Result breaks `Future` bounds). Controlled per-module conversion proceeds on `main` instead (green at every commit). DONE so far: `src/shuffle.rs` (`permute_with_seed` → `Result<()>`, `211c34f`); `src/bench/progress_snapshot.rs` (`from_samples` → `Result<Self>`, `bb34ab0`). Hazard count 318 → 291. Findings: (a) most hot-spot hazards are *intentional infallible* `saturating_*`/`unwrap_or` clamped arithmetic (formatters, snapshot builders) — converting them to Result is largely ceremony + TODO markers; (b) conversions cascade into trait walls (trait methods returning non-Result, `FnMut` callbacks) handled per phase-3 with boundary `unwrap_or_else` + visible `TODO(rs-cleanup)`; (c) genuinely-fallible fns (IO/parsing in migrate/cargo-rudzio/config) are the higher-value targets. REMAINING: render.rs, bench/report.rs, runner.rs, config.rs, migrate/{emit,rewrite}.rs, cargo-rudzio/generate.rs, common/time.rs + others. Per-file workflow proven: M-1 `--write` → `# Errors` doc + drop `#[must_use]` → caller `?`/boundary-TODO → clippy+test → commit. TWO CRITICAL FINDINGS (`.opencode/issues.md` 2026-05-29): (1) `clippy::unnecessary_wraps` (denied) rejects Result-wrapping of *private* total fns — only bare-`pub` fns are exempt; M-1 gained `--pub-only` (`8d67efa`) to convert only clippy-safe public fns. (2) The hot-spot hazards are overwhelmingly *intentional infallible idioms* (`saturating_*`/`unwrap_or`/`.get().unwrap_or()`) that the workspace's OWN lints (`arithmetic_side_effects`/`indexing_slicing`/`unwrap_used` = forbid) prescribe — NOT hidden failures. Formatters, tolerant parsers (`Config::from_argv_and_env` never errors), and integer stat-math are total by design. Mechanical "every fn → Result" conflicts with clippy for the total-fn majority and adds zero-safety-gain ceremony for public ones. REVISED scope: convert only fns with a genuine failure path (mostly already Result); leave intentional-total fns non-Result (linter report + inline TODOs already satisfy "visible not hidden"); per-fn convert a hazard to `.context()?`/`ensure!` only where out-of-range genuinely means corruption. RECOMMENDATION: add an intentional-total allowlist to non-result-fns (mirror expect-allow-ban) to green the gate without artificial wrapping. Awaiting user steer. | IN PROGRESS | `211c34f`+`bb34ab0`+`8d67efa` (wip: `5d4ab70`) |
| 19 | M-3: cosmetic pass | PENDING | TBD |
| 20 | Phase 2: TODO marker sweep | PENDING | TBD |

Last updated: 2026-05-28 (Phase B framework in progress). Future-PP MUST update this matrix as work progresses; commit hash MUST be filled before `git push`.

## Goal

Two phases:

1. **Phases 0a + 0b** — fix two stale-state bugs (#17 silent bin-mount fallthrough, #18 silent aggregator cache reuse) reported by downstream consumers. Both are members of the Bug #16 defect family: silent failure produced by an incorrect invariant. One batched commit covering both.

2. **Result hazard cleanup** — convert every non-Result-returning fn in the workspace to return `anyhow::Result<T>`. Pre-work: audit `#[expect]` attributes (P-0), remove `Default` impls (P-1), retype the two `impl Iterator<Item=...>` return-position users (P-3), extend the linter (P-4). Tool work: hybrid syn-based sig transformer (M-1). Main work: per-module PRs (M-2) + cosmetic pass (M-3). Phase 2: sweep TODO markers.

Final state: linter gate `non-Result fn count = 0` + `TODO(rs-cleanup) count = 0` + `#[expect] count` reviewed + all hazards propagated through Result.

## Architectural change in one sentence (cleanup phase)

**Every function in the rudzio workspace that today returns a non-Result type starts returning `anyhow::Result<T>`. Wrap-by-default. Trait signatures locked by std (Display/Debug/PartialEq/Hash) excluded. Existing `#[expect]` attributes audited and reduced. Manual `Default` impls and `#[derive(Default)]` usages removed where viable. Caller-side `?` propagation is the universal pattern; `.with_context(...)` adds error provenance.**

---

## Decisions Log

All locked via in-conversation `AskUserQuestion` ratification. Future-PP executes per these; any deviation requires user re-approval before action.

### Phase 0 (bug fixes)

| ID | Question | Decision |
|---|---|---|
| **D-17.1** | Bug #17 reproducer test placement | Extend `bug_16_bin_source_tests_invisible_spec.rs` with two new tests. |
| **D-17.2** | Edge case `src/main.rs + src/lib.rs` (lib-dir-shared) coverage | Explicit third test — package with both `src/main.rs` AND `src/lib.rs` containing rudzio tests. |
| **D-18.1** | Bug #18 stamp granularity | Version + timestamp at build time only: `<CARGO_PKG_VERSION>-<epoch_seconds>`. **NEW (v2 revision)**: drop `cargo:rerun-if-changed=build.rs` from build.rs — the audit showed it was preventing the stamp from updating on cargo-rudzio source edits. Default cargo policy (rerun on any src/ change) restored. |
| **D-18.2** | Bug #18 dev escape hatch | No escape hatch. |
| **D-18.3 (NEW v2)** | Bug #18 invalidation mechanism | **Subdir-hash approach (option c)** — `aggregator_subdir_name` includes the build stamp hash. No sentinel file. No wipe. Different stamp → different subdir; old dirs orphaned and harmless. Race-free for concurrent invocations. Manual GC via `rm -rf <target>/rudzio-auto-runner-*` documented for users. |
| **D-PH0** | Phase 0a + 0b sequencing | One batched commit covering both fixes + all new tests. **REVISED (v3)**: commit now covers 0a + 0b + 0c (linter added) per user direction. |

### Phase 0c — Expect/Allow Ban Linter (NEW v3)

| ID | Question | Decision |
|---|---|---|
| **D-EXP.1** | Scope of `#[expect]` detection | Flag ALL `#[expect(...)]` uniformly. No special case for `unsafe_code` (user handles separately via planned consolidation crate). |
| **D-EXP.2** | Baseline handling | Fail-fast: every existing site fails CI until removed or per-site-justified allowlist entry added. NO auto-baseline. |
| **D-EXP.3** | File scope | Workspace-wide. All `.rs` files. Excludes (path-skip): `target/`, `migrate/fixtures/` (golden goldens), `fixtures/tests/fixtures/` (trybuild deliberate inputs), `linters/expect-allow-ban/tests/fixtures/` (self-test inputs). |
| **D-EXP.4** | Production `unsafe_code` sites disposition | No pre-allowlist. Sites fail loudly until user addresses (move to consolidation crate or add justified allowlist entry). |
| **D-EXP.5** | Unsafe consolidation crate | Out of scope for current campaign. Tracked in `.opencode/issues.md` for future planning. |
| **D-EXP.6** | `#[allow]` detection | Twin brother of `#[expect]`. Detected uniformly. Same rules, same allowlist schema. |
| **D-EXP.7** | CI landing state | Linter ships with EMPTY allowlist. CI step goes RED on `check-lints` after landing — this is a FEATURE not a bug. Per-site cleanup is follow-up work. |
| **D-EXP.8** | CI isolation | Failing linter must NOT block other CI checks. Justfile `check-lints` runs all linters even if one fails (accumulates exit code). CI workflow runs linters as a separate parallel job from main `ci` job. |

### Phase 1+ (cleanup)

| ID | Question | Decision |
|---|---|---|
| **D-1** | Trait-impl-locked fns | Display::fmt auto-excluded (returns `fmt::Result`). Default/Clone are user-flagged design smells: remove Default impls + derives workspace-wide; **P-2 DELETED** (audit found zero manual `impl Clone for T`). |
| **D-2** | Iterator policy | Originally "retype every Iterator Item uniformly to Result<T, Error>". **REVISED (v2)**: audit found zero `impl Iterator for T` in workspace. Only two `impl Iterator<Item=...>` *return positions* (`src/output/render.rs:1251`, `cargo-rudzio/src/generate.rs:2686`). Retype only those two return positions' Item type if they yield error-prone values; otherwise document why they're safe. Scope was massively overstated; ripple is minimal. |
| **D-3** | Scope phasing | All 576 non-Result fns in lockstep. Linter gate tightens from `hazards = 0` → `non-Result count = 0` once main conversion lands. |
| **D-4** | Tooling | Hybrid syn-based transformer at `linters/rs-result-sig-rewrite/`. **REVISED (v2)**: explicit edge-case policy (async fn / const fn / where clauses / macros / `-> !` / `-> impl Trait` / closure-returns) documented in M-1 below. |
| **D-5** | Default + Clone timing | Include in pre-work, before main-conversion work. Clone audit step removed (P-2 deleted per D-1 v2). |
| **D-6** | FallibleIterator crate | SUPERSEDED by D-2 v2. No external crate needed; scope minimal. |
| **D-7** | Caller scope per PR | In-tree only. CHANGELOG.md per minor version bump. |
| **D-8** | Tool location | `linters/rs-result-sig-rewrite/`. |
| **D-9** | Per-item error default | Propagate-and-stop. (Largely moot per D-2 v2.) |
| **D-10** | Wave 1 agent launch pattern | Audits parallel (3), then tooling parallel (2). **REVISED (v2)**: only 3 of the 5 wave-1 items remain meaningful — P-0 (NEW), P-1 audit (Default sites), and M-1 (tool). P-2 deleted; P-3 reduced to 2-line manual inspection. P-4 sequenced AFTER P-0/P-1 (extension brief depends on their findings). |
| **D-11** | Progress-file contract | Strict. Missing or malformed → discard worktree. |
| **D-12** | Agent verification | Write-only, human verifies. Agents never run cargo. |
| **D-13** | Cargo invocation policy | All cargo commands run in this session only. Agents shall not call `cargo` anything (build/check/test/clippy/fmt/audit/deny/run/metadata/tree/expand/udeps). |
| **D-14** | Tool-binary invocation by agents | Allowed. **REVISED (v2)**: agent briefs MUST include absolute paths to the tool binary (e.g. `/var/home/.../linters/target/debug/rs-result-sig-rewrite`) and absolute file paths for the target file. Tool accepts `--file <abs-path>` CLI arg. |
| **D-15** | Worktree target dir | Fresh `target/` per worktree, stored on `/var/mnt/data-cache/`. **REVISED (v2)**: directory at `/var/mnt/data-cache/cargo/worktree-targets/<worktree-id>/` must be created upfront in this session before any agent launches; cleanup policy on worktree disposal documented below. Agents do NOT source `.opencode/cargo-env.sh` (which would clobber the per-worktree dir). |
| **D-16 (NEW v2)** | `#[expect]` attribute policy | Existing `#[expect]` are red flags themselves and audited in P-0. Going forward, `#[expect]` is NOT the escape hatch for intentional clamps (custom linters can't be `#[expect]`-suppressed; clippy `forbid` lints can't be `#[expect]`-ed either — compile error). Replacement: file-level allowlist at `linters/non-result-fns/allowlist.toml` listing fully-qualified fn paths + written justification; the linter reads it and excludes those entries from the gate. |
| **D-17 (NEW v2)** | Phase 0a narrow non-Cargo-convention regression | The `[[bin]] name = "foo", path = "src/foo/foo.rs"` layout (parent name == bin name, but file_name ≠ `main.rs`) is NOT a Cargo idiom. We accept the regression: post-fix this layout falls into flat-style. No proactive test. Document in CHANGELOG. |
| **D-18 (NEW v2)** | Plan revision approach for gap findings | Apply all recommended revisions in PLAN.md v2 (this document); user reviews; new green light required before any execution starts. |

---

## Constraints carried from prior session

- Workspace lints strict: `arbitrary_source_item_ordering` forbid, `allow_attributes` forbid, `arithmetic_side_effects` forbid, `use_debug` forbid, `dbg_macro` forbid, `indexing_slicing` forbid, `unsafe_code` deny, full `correctness`/`complexity`/`perf`/`style` groups at forbid.
- `linters/non-result-fns/` is a hard CI gate: `just check-lints` must exit 0. Currently 318 hazards in 108 non-Result fns.
- `linters/BASELINE.md` is the committed snapshot at commit `ce2f5d8`.
- `/var/mnt/data-cache/` for all cargo + tmp; `/var/home/` constrained (~13G free, sometimes 100% used).
- Working dir: `/var/home/fathomio/Projects/PorfiryPetrovich-ai/feat/platform-backend/shim/Projects/mykytanikitenko/rudzio/main`.
- Source `.opencode/cargo-env.sh` before every cargo call **in this session only**. Agents do NOT source this.
- Shell-tool max timeout 120 000 ms; long-running test must background via setsid + log polling. **NEW (v2)**: see "Verification gate execution policy" below for how each gate handles the 120s limit.
- `CARGO_NET_OFFLINE=true` (not `"1"`).
- Git safety: NEVER amend pushed commits; NEVER `--force` to main; NEVER `--no-verify`.

---

## Phase 0a — Bug #17 fix (REVISED v2)

### Defect statement

`cargo-rudzio/src/generate.rs:collect_unmounted_bin_entries` (committed at `261fa8a`, modified by `d08826d`) decides directory-style vs flat-style bin mounting based on `parent.file_name() == bin.name`. Wrong invariant.

Cargo's directory-style convention is:
1. Entry file basename = `main.rs`
2. Parent dir is non-shared (specifically, not the same as `[lib]`'s parent dir)

NOT the `[[bin]] name` attribute matching parent dir name.

For `[[bin]] name = "<prefix>-<role>", path = "src/bin/<role>/main.rs"`: parent name `<role>` ≠ bin name `<prefix>-<role>` → falls into flat-style → walks only `main.rs` → submodule markers silently invisible.

### Fix shape (exact)

In `collect_unmounted_bin_entries`, replace `parent_name_matches` with `is_directory_style_bin`:

```rust
let lib_dir = member.src_lib_path.as_ref().and_then(|lib| lib.parent());
let is_directory_style_bin =
    bin.src_path.file_name().and_then(|n| n.to_str()) == Some("main.rs")
        && bin.src_path.parent() != lib_dir;
```

Both signals together:
1. `file_name == "main.rs"` — Cargo's actual directory-style entry marker.
2. `parent != lib's parent` — guards `src/main.rs + src/lib.rs` against double-mount of shared submodules.

Standalone check + test-surface check both key on `is_directory_style_bin` instead of `parent_name_matches`.

### Accepted regression (D-17)

`[[bin]] name = "foo", path = "src/foo/foo.rs"` (parent name matches bin name, but file_name ≠ `main.rs`) falls into flat-style post-fix. Pre-fix it was incorrectly treated as directory-style. Non-Cargo-idiomatic layout. Accept the regression; document in CHANGELOG.

### Edge-case coverage matrix

| Layout | file_name | parent | lib_dir | result | Pre-fix | Post-fix |
|---|---|---|---|---|---|---|
| `src/main.rs` + `src/lib.rs` | `main.rs` | `src/` | `src/` | flat | flat | flat (preserved) |
| `src/main.rs` (bin only) | `main.rs` | `src/` | None | dir-style | flat | dir-style (improved) |
| `src/bin/server/main.rs`, name `"server"` | `main.rs` | `src/bin/server/` | `src/` | dir-style | dir-style | dir-style (preserved) |
| `src/bin/server/main.rs`, name `"file-v3-server"` | `main.rs` | `src/bin/server/` | `src/` | dir-style | **flat (Bug #17)** | **dir-style (FIXED)** |
| `src/bin/foo.rs` (flat) | `foo.rs` | `src/bin/` | `src/` | flat | flat | flat (preserved) |
| `src/foo/foo.rs` (non-idiomatic) | `foo.rs` | `src/foo/` | `src/` | flat | dir-style | flat (accepted regression D-17) |
| `extras/server/main.rs` (custom path) | `main.rs` | `extras/server/` | `src/` | dir-style | dir-style or flat depending on name | dir-style |
| `fixtures/src/bin/passing_tokio_mt.rs` (dogfood) | `passing_tokio_mt.rs` | `fixtures/src/bin/` | `fixtures/src/` | flat → standalone → skip | same | same (preserved) |

### Files edited

- `cargo-rudzio/src/generate.rs` — `collect_unmounted_bin_entries` heuristic correction. The function is at approximately line 4013 in HEAD `d08826d`. Verify exact location during execution.

### Tests added (3 new, all in `cargo-rudzio/tests/bug_16_bin_source_tests_invisible_spec.rs`)

The spec uses `anyhow::ensure!` on stdout substrings (NOT the `InvariantOutcome` classifier from surface_visibility spec). All new tests inherit this style.

**Test 1 (existing, unchanged)**: `rudzio_test_inside_bin_source_runs_under_cargo_rudzio_test`. `[[bin]] name = "server"`. Pre-fix passes (since step 3); post-fix still passes.

**Test 2 (NEW)**: `rudzio_test_inside_bin_with_mismatched_name_runs_under_cargo_rudzio_test`

```rust
// Fixture: same shape as Test 1 but [[bin]] name = "myproject-server"
// Marker: bin_source_test in src/bin/server/ops.rs
// Assertion (bug_16 substring style):
anyhow::ensure!(
    stdout.contains("bin_source_test"),
    "Bug #17: bin_source_test absent from cargo rudzio test output. \
     The fix must mount src/bin/server/ops.rs regardless of [[bin]] name. \
     stdout = {stdout:?}\nstderr = {stderr:?}"
);
anyhow::ensure!(
    stdout.contains(" 2 passed;"),
    "Expected 2 passed (lib_baseline_test + bin_source_test). stdout = {stdout:?}"
);
```

Pre-fix: stdout shows " 1 passed;" + missing bin_source_test → FAILS. Post-fix: " 2 passed;" + bin_source_test present → PASSES.

**Test 3 (NEW)**: `rudzio_tests_in_default_bin_main_rs_and_lib_rs_run_exactly_once_each`

Fixture uses `src/main.rs` + `src/lib.rs` sharing a `mod common;` (which `src/common.rs` defines, containing a rudzio test). This exposes the real double-mount surface: if the heuristic walked `src/` from main.rs's parent without the lib_dir guard, common::test would register twice.

Assertion uses occurrence counting:

```rust
let occurrences = stdout.matches("common_shared_test").count();
anyhow::ensure!(
    occurrences == 1,
    "common_shared_test ran {occurrences} times; expected exactly 1. \
     If 2: double-mount via shared mod indicates lib_dir guard regression. \
     If 0: lib mount path is broken. stdout = {stdout:?}"
);
anyhow::ensure!(
    stdout.contains(" 1 passed;") || stdout.contains(" 2 passed;"),
    "Unexpected test count. stdout = {stdout:?}"
);
```

Pre-fix: passes (parent_name_matches = false → flat → no double-walk). Post-fix without guard: would double-mount (FAIL). Post-fix with guard: passes (flat-style branch).

**Test 4 (NEW)**: `rudzio_test_in_deeply_nested_bin_submodule_runs_under_cargo_rudzio_test`

Fixture: `[[bin]] name = "myproject-server"`, path = `src/bin/server/main.rs`. main.rs has `mod api;`. `src/bin/server/api.rs` has `mod handler;`. `src/bin/server/api/handler.rs` has the rudzio test marker `deeply_nested_test`.

```rust
anyhow::ensure!(
    stdout.contains("deeply_nested_test"),
    "Recursive walk in walk_dir_for_test_markers_and_main must reach depth 3+. \
     stdout = {stdout:?}\nstderr = {stderr:?}"
);
```

Pre-fix: silent (flat-style only mounts main.rs). Post-fix: passes (dir-style walks recursively).

### Phase 0a verification gates

1. Test 1 (existing) still passes.
2. Test 2 (mismatched name) passes.
3. Test 3 (default-bin shared-mod) passes.
4. Test 4 (nested helper) passes.
5. `surface_visibility_invariant_spec` (6 tests) still passes.
6. `silent_member_drop_invariant_spec` (1 test) still passes.
7. Dogfood: `runtime_group_key_invariants` (4) + `bug_8_helper_discovery_edge_cases_spec` (5) + `per_binary_helper_isolation_spec` (1) + `path_attribute_helper_spec` (1) all pass.
8. Clippy clean per `cargo clippy -p cargo-rudzio --no-deps --all-features --all-targets -- -D warnings`.

---

## Phase 0b — Bug #18 fix (REVISED v2: option c subdir-hash)

### Defect statement

`<aggregator>/target/` is preserved across `cargo rudzio test` invocations. The aggregator binary at `<aggregator>/target/debug/rudzio-auto-runner` is keyed by cargo's fingerprinter on source-content hashes — none of which include cargo-rudzio's binary version. When cargo-rudzio is rebuilt and generates byte-identical aggregator source for unchanged consumer workspace, cargo skips relink → old binary persists with old runner code.

### Original sentinel-wipe approach (option d) rejected

Agent audit found two critical issues:

1. **Stamp granularity broken**: `cargo:rerun-if-changed=build.rs` PREVENTS the stamp from re-evaluating on cargo-rudzio source edits. Defeated its own purpose. Dev iteration scenario uncaught.

2. **Concurrency race**: `aggregator_subdir_name` (`generate.rs:749-758`) hashes only `workspace_root`. Two parallel `cargo rudzio test` against same workspace = same dir. Whole-dir wipe races with sibling's in-flight cargo build → deleted rlibs, killed linker, corrupted target. The very race class `generate.rs:3108-3109` already documents.

Plus `remove_dir_all` macOS ENOTEMPTY race that existing code explicitly avoids.

### Revised approach (option c): hash stamp into subdir name

**1. `cargo-rudzio/build.rs` (new file) captures build stamp**

```rust
// cargo-rudzio/build.rs (NEW)
fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format!("{version}-{epoch}");
    println!("cargo:rustc-env=CARGO_RUDZIO_BUILD_STAMP={stamp}");
    // NOTE: NO rerun-if-changed directives. The cargo default is "rerun
    // build.rs whenever a source file in the package changes" which is
    // exactly what we want — any change to cargo-rudzio source triggers
    // a fresh stamp. Adding `rerun-if-changed=build.rs` would OVERRIDE
    // that default and lock the stamp to build.rs changes only, which
    // was the broken behavior in the v1 plan.
}
```

**2. `cargo-rudzio/src/generate.rs` reads stamp + uses it in subdir name**

```rust
const BUILD_STAMP: &str = env!("CARGO_RUDZIO_BUILD_STAMP");
```

Modify `aggregator_subdir_name` (currently around `generate.rs:749`, verify location during execution):

```rust
// BEFORE:
// fn aggregator_subdir_name(plan: &Plan) -> String {
//     let hash = fnv1a64(plan.workspace_root.to_string_lossy().as_bytes());
//     format!("{AGGREGATOR_NAME}-{hash:016x}")
// }

// AFTER (v2):
fn aggregator_subdir_name(plan: &Plan) -> String {
    let workspace_hash = fnv1a64(plan.workspace_root.to_string_lossy().as_bytes());
    let stamp_hash = fnv1a64(BUILD_STAMP.as_bytes());
    format!("{AGGREGATOR_NAME}-{workspace_hash:016x}-{stamp_hash:016x}")
}
```

**3. Stderr note on first invocation (any cargo-rudzio version change creates a new dir)**

In `write_runner` or the cargo-rudzio CLI entrypoint, emit a one-line stderr note when the directory at the new subdir name doesn't exist yet AND any sibling `{AGGREGATOR_NAME}-{workspace_hash:016x}-*` dirs DO exist (signal that a previous cargo-rudzio version's dir is orphaned):

```rust
// Pseudo: in write_runner before fs::create_dir_all(&out_dir)
if !out_dir.exists() {
    let target_root = out_dir.parent().expect("out_dir has parent");
    let workspace_prefix = format!("{AGGREGATOR_NAME}-{workspace_hash:016x}-");
    let orphan_count = fs::read_dir(target_root)
        .ok()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&workspace_prefix)
                })
                .count()
        })
        .unwrap_or(0);
    if orphan_count > 0 {
        eprintln!(
            "cargo-rudzio: detected {orphan_count} orphan aggregator dir(s) under {} \
             (left by previous cargo-rudzio versions). Safe to clean with: \
             rm -rf {}/*",
            target_root.display(),
            target_root.display()
        );
    }
}
```

### Why this approach is race-free

- Two parallel invocations from the SAME cargo-rudzio binary → same stamp → same subdir → use the existing per-file cleanup logic (already race-safe per `generate.rs:3108-3109` comment).
- Two parallel invocations from DIFFERENT cargo-rudzio binaries → different stamps → different subdirs → no shared state.
- No `remove_dir_all` on a live target/ dir.

### Edge-case handling

- **First-time run, no `<aggregator>/` dir exists**: create dir + proceed. No special handling.
- **Run after cargo-rudzio bump**: new stamp → new subdir name → fresh dir created → previous dir orphaned (sibling dir under same target root). Stderr message names the orphan count.
- **Concurrent invocations**: covered above.
- **Manual GC**: `rm -rf <target>/rudzio-auto-runner-*` (workspace-wide nuke) or scripted glob keeping the most recent dir per workspace.

### Files edited (Phase 0b)

- `cargo-rudzio/build.rs` — NEW file.
- `cargo-rudzio/src/generate.rs` — `BUILD_STAMP` const + `aggregator_subdir_name` modification + optional orphan-detection stderr note in `write_runner`.

### Tests added (Phase 0b)

NEW spec: `cargo-rudzio/tests/bug_18_aggregator_cache_invalidation_spec.rs`. One behavioral test:

**Test: `cargo_rudzio_test_creates_distinct_aggregator_subdir_when_stamp_changes`**

This test exercises the subdir-hash mechanism. Strategy: detect that DIFFERENT build stamps yield DIFFERENT subdir paths.

Approach (since we can't easily simulate a cargo-rudzio rebuild within a test):

1. Build a synthetic consumer workspace.
2. Run `cargo rudzio test` once — captures the current cargo-rudzio binary's stamp via the aggregator subdir name.
3. Glob `<consumer_target>/rudzio-auto-runner-*` — expect exactly 1 match. Bind path.
4. Extract the stamp-hash portion of the subdir name.
5. Compute what the subdir name WOULD be with a different (synthetic) stamp `0.0.0-99999` — `format!("{AGGREGATOR_NAME}-{workspace_hash:016x}-{synthetic_stamp_hash:016x}")` — assert this path is DIFFERENT from the observed path.

The assertion is structural (the subdir naming function bakes the stamp into the path), not behavioral. A regression that drops the stamp from the subdir name would make the test fail because both paths would be identical.

```rust
let glob_pattern = format!("{}/rudzio-auto-runner-*", consumer_target.display());
let matches: Vec<PathBuf> = glob::glob(&glob_pattern)?
    .filter_map(Result::ok)
    .collect();
anyhow::ensure!(
    matches.len() == 1,
    "Expected exactly 1 aggregator subdir, found {}: {matches:?}",
    matches.len()
);
let observed_path = &matches[0];
let observed_name = observed_path
    .file_name()
    .and_then(|n| n.to_str())
    .expect("subdir has a name");
// AGGREGATOR_NAME = "rudzio-auto-runner"; format is "<NAME>-<wshash>-<stamphash>"
let parts: Vec<&str> = observed_name.rsplitn(3, '-').collect();
anyhow::ensure!(
    parts.len() == 3,
    "Expected subdir name format 'rudzio-auto-runner-<wshash>-<stamphash>'; got {observed_name:?}. \
     If only 1 hash component, Bug #18 regression: stamp not in subdir name."
);
let stamp_hash_observed = parts[0];
// Compute synthetic alternative — what the subdir would be with a different stamp.
let synthetic_stamp_hash = format!("{:016x}", fnv1a64("0.0.0-99999".as_bytes()));
anyhow::ensure!(
    stamp_hash_observed != synthetic_stamp_hash,
    "Observed stamp hash equals synthetic alternative — unexpected collision or test fixture wrong"
);
// Test passes: the subdir name distinguishes the current cargo-rudzio's stamp
// from a hypothetical alternative. Bug #18 invariant pinned.
```

This is a regression guard. A future change that drops the stamp from the subdir name fails the structural format check.

### Phase 0b verification gates

1. `cargo build -p cargo-rudzio` succeeds (build.rs runs).
2. Inspect built binary: `strings target/debug/cargo-rudzio | grep CARGO_RUDZIO_BUILD_STAMP` shows the stamp.
3. New bug_18 test passes.
4. Existing tests still pass: bug_16 (3 tests after Phase 0a), bug_17 (NEW, 4 tests), surface_visibility (6), silent_member_drop (1).
5. Dogfood unchanged: runtime_group_key_invariants + generate_spec etc.
6. Clippy clean.
7. Sanity check (manual, in this session): run `cargo run -p cargo-rudzio -- test runtime_group_key_invariants` twice in a row. Both runs should reuse the same aggregator subdir (same stamp, same workspace). NO new orphan dir created between runs.

### Phase 0b risks (revised)

- **Old aggregator dirs accumulate**: per cargo-rudzio version change, one new dir per workspace. Stderr note mentions count and cleanup command. No automatic GC in this fix; document. Future work could add `cargo rudzio clean`.
- **First migration**: on the first run after this fix lands, every workspace gets a new aggregator dir alongside the old one. One-time cost. Loud.
- **NO concurrency race**: subdir-hash approach eliminates the race entirely.
- **NO stamp-granularity bug**: dropping `rerun-if-changed=build.rs` restores cargo default.

---

## Phase 0c — Expect/Allow Ban Linter (NEW v3)

### Why this phase exists

User directive (this session): existing `#[expect(...)]` and `#[allow(...)]` attributes are likely LLM-authored escape hatches that bypassed code review. They are red flags themselves. The linter must:

1. Detect every `#[expect(...)]` and `#[allow(...)]` real attribute (inner + outer) workspace-wide.
2. Treat `#[expect]` and `#[allow]` uniformly (twin brothers per D-EXP.6).
3. Apply no special case for `unsafe_code` — user plans separate consolidation crate for unsafe code (out of scope for current campaign, per D-EXP.4/D-EXP.5).
4. Fail CI loudly on every site that isn't pre-justified in `allowlist.toml`.
5. NOT block other CI checks from running (fmt, clippy, audit, deny, tests must execute regardless of linter exit code) per D-EXP.8.

Existing `reason = "..."` strings on attrs are NOT auto-extracted into an allowlist baseline. User quote: "my intuition it was added by llm as escape hatch, so we shall fail on all of them loudly first". Each site requires per-site review post-landing.

### Linter design

- **Crate**: `linters/expect-allow-ban/` — binary crate, sibling of `non-result-fns/`.
- **Deps**: `syn` + `proc-macro2` + `walkdir` (existing `linters/Cargo.toml` workspace deps) + `serde` + `toml` (NEW workspace deps to be added).
- **Walk**: all `.rs` files via `walkdir` from repo root (CWD of `just check-lints` invocation).
- **Skip dirs (built-in)**: `target/`, `migrate/fixtures/`, `fixtures/tests/fixtures/`, `linters/expect-allow-ban/tests/fixtures/`.
- **Per file**: `syn::parse_file` → `syn::visit::Visit` walking `Attribute` nodes. Auto-ignores comments + macro-emitted token-stream string contents (syn doesn't see strings as attrs — clean filter for free).
- **Detect**: `attr.path().is_ident("expect") || attr.path().is_ident("allow")` — both outer `#[…]` (`AttrStyle::Outer`) and inner `#![…]` (`AttrStyle::Inner`).
- **Allowlist**: reads optional `linters/expect-allow-ban/allowlist.toml` (ships EMPTY).
- **Match**: exact `path` (repo-relative) + `line` + `kind` (`expect`|`allow`) + `lints` set.
- **Orphans**: allowlist entries with no matching attr → also reported as failure (forces user to clean stale entries).
- **Output**: detailed report to `linters/expect-allow-ban-report.txt`; summary counts to stdout.
- **Exit**: non-zero iff any unmatched attr OR orphan allowlist entry exists.

### Allowlist schema (ships empty)

```toml
# linters/expect-allow-ban/allowlist.toml
#
# Each entry pre-justifies one `#[expect(...)]` or `#[allow(...)]`
# attribute in the workspace. Linter fails CI on attributes NOT
# present here. Entries reference attributes by exact file + line.
#
# Per D-EXP.2/D-EXP.7, this file ships empty: every existing attr
# fails CI until either removed (fix the code) or pre-justified
# here with a vetted, human-authored reason.
#
# Schema:
# [[entry]]
# path = "src/runner.rs"        # repo-relative
# line = 794
# kind = "expect"               # "expect" | "allow"
# lints = ["unsafe_code"]       # exact set, order-insensitive
# reason = "<vetted explanation, not auto-extracted>"
```

### Justfile changes (D-EXP.8 isolation)

`check-lints` recipe rewritten to bash-shebang form. Both linters run regardless of intermediate failures; final exit code reflects worst result.

```just
# Run all custom code-audit linters under `linters/`.
#
# Each linter is its own binary crate. Both run unconditionally even
# if a predecessor fails — the recipe accumulates failures and exits
# non-zero at the end. This means a failing linter does NOT mask
# failures in subsequent linters.
#
# Per D-EXP.8, `check-lints` is intentionally REMOVED from the `ci`
# aggregate so a linter failure doesn't block fmt/clippy/audit/deny/
# test from executing. The CI workflow runs `check-lints` as a
# separate parallel job.
check-lints:
    #!/usr/bin/env bash
    set -uo pipefail
    status=0
    cargo run --manifest-path linters/Cargo.toml -p non-result-fns || status=$?
    cargo run --manifest-path linters/Cargo.toml -p expect-allow-ban || status=$?
    exit $status
```

`ci` aggregate updated to remove `check-lints`:

```just
ci: check-fmt check-taplo check-clippy check-audit check-deny test
```

### CI workflow changes (D-EXP.8 isolation)

`.github/workflows/ci.yml` gains a second job `lints` that runs in parallel with the existing `ci` job. Neither blocks the other.

```yaml
jobs:
  ci:
    name: just ci
    runs-on: [self-hosted, linux, hetzner-builder, nix, rust]
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: just ci
        run: nix develop --extra-experimental-features 'nix-command flakes' --command just ci

  lints:
    name: just check-lints (linter gate)
    # Independent of `ci`. Per D-EXP.7 this job is expected to be RED
    # until per-site allowlist entries are written or attrs removed.
    # The other CI gates run regardless of this job's outcome.
    runs-on: [self-hosted, linux, hetzner-builder, nix, rust]
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - name: just check-lints
        run: nix develop --extra-experimental-features 'nix-command flakes' --command just check-lints
```

### Files added (Phase 0c)

- `linters/expect-allow-ban/Cargo.toml`
- `linters/expect-allow-ban/src/main.rs`
- `linters/expect-allow-ban/allowlist.toml` (empty content + schema doc comment)

### Files modified (Phase 0c)

- `linters/Cargo.toml` — add `expect-allow-ban` to `members`; add `serde` + `toml` to workspace deps
- `Justfile` — `check-lints` recipe rewritten + `ci` aggregate updated
- `.github/workflows/ci.yml` — add parallel `lints` job
- `.opencode/issues.md` — entry: "Phase 0c landed CI red; expect/allow review pending site-by-site"

### Phase 0c verification gates

1. `cargo build -p expect-allow-ban` succeeds clean (this session only).
2. Linter binary runs against workspace; reports findings; exits non-zero (CI RED expected per D-EXP.7).
3. `non-result-fns` linter still passes its threshold (unchanged in Phase 0c).
4. Justfile `just check-lints` execution: both linters run; recipe exits with the worst status.
5. CI workflow shape verified — `lints` job structurally separate from `ci` job.
6. Production tests (`generate_spec` etc.) still green; not affected by linter changes.

### Out of scope for Phase 0c

- Fixing or allowlisting any existing attrs (deferred to follow-up — user reviews CI red output per-site).
- `unsafe_code` consolidation crate (D-EXP.5; tracked in `.opencode/issues.md`).
- `rudzio::test` macro return-type loosening (smell logged — many `clippy::unnecessary_wraps` suppressions root here).

---

## Phase 0 commit strategy (covers 0a + 0b + 0c)

Per D-PH0, one batched commit.

### Commit content

Files staged:
- `cargo-rudzio/src/generate.rs` (Phase 0a heuristic + Phase 0b stamp const + subdir name)
- `cargo-rudzio/build.rs` (NEW, Phase 0b stamp)
- `cargo-rudzio/tests/bug_16_bin_source_tests_invisible_spec.rs` (Phase 0a tests 2, 3, 4)
- `cargo-rudzio/tests/bug_18_aggregator_cache_invalidation_spec.rs` (NEW, Phase 0b)
- `cargo-rudzio/tests/main.rs` (mount `mod bug_18_aggregator_cache_invalidation_spec;`)
- `linters/expect-allow-ban/Cargo.toml` (NEW, Phase 0c)
- `linters/expect-allow-ban/src/main.rs` (NEW, Phase 0c)
- `linters/expect-allow-ban/allowlist.toml` (NEW, Phase 0c — empty content + schema doc comment)
- `linters/Cargo.toml` (Phase 0c — add member + serde/toml deps)
- `Justfile` (Phase 0c — `check-lints` rewrite + `ci` aggregate update)
- `.github/workflows/ci.yml` (Phase 0c — add parallel `lints` job)
- `.opencode/issues.md` (Phase 0c entry: CI-red status, design smells)
- `PLAN.md` (this document, marked as commit-time snapshot)

### Verification before commit (executed in this session)

In this order. ALL must pass before commit:

1. `source .opencode/cargo-env.sh && cargo check -p cargo-rudzio --tests`
2. `source .opencode/cargo-env.sh && cargo clippy -p cargo-rudzio --no-deps --all-features --all-targets -- -D warnings`
3. `source .opencode/cargo-env.sh && cargo run -p cargo-rudzio -- test bug_16_bin_source_tests_invisible_spec`
4. `source .opencode/cargo-env.sh && cargo run -p cargo-rudzio -- test bug_18_aggregator_cache_invalidation_spec`
5. `source .opencode/cargo-env.sh && cargo run -p cargo-rudzio -- test surface_visibility_invariant_spec`
6. `source .opencode/cargo-env.sh && cargo run -p cargo-rudzio -- test silent_member_drop_invariant_spec`
7. `source .opencode/cargo-env.sh && cargo run -p cargo-rudzio -- test runtime_group_key_invariants`
8. `source .opencode/cargo-env.sh && cargo run -p cargo-rudzio -- test generate_spec` (background via setsid + log polling — likely >120s).

For commands likely to exceed 120s (gate 8): use `cargo run ... > /tmp/log 2>&1 &` pattern in shell, then poll `/tmp/log` periodically, or use `Bash` tool's `run_in_background: true`.

### Commit message template

```
fix(#17/#18): correct bin-mount heuristic + aggregator subdir invalidation

Two related stale-state bugs, single batched commit.

Bug #17 (silent bin-mount fallthrough):
[paragraph explaining heuristic correction + matrix]

Bug #18 (silent aggregator cache reuse):
[paragraph explaining subdir-hash approach + why option c over option d]

Tests:
- bug_16 spec: +3 tests (mismatched name; default-bin shared mod; nested helper)
- bug_18 spec: 1 test (structural subdir-name regression guard)

CHANGELOG: pre-0.2.0 entry noting the accepted regression for
non-Cargo-idiomatic `[[bin]] path = "src/foo/foo.rs"` layout (D-17 v2).

Verification: full proactive suite + dogfood + clippy green.
```

### Push

After successful commit: `git push origin HEAD`.

---

# Phase 1+ (Result hazard cleanup) — REVISED v2

Detailed sequence executes only after Phase 0 lands cleanly. The user provides a separate green light to begin.

## Pre-work order

### P-0 (REVISED v3) — Per-site review of `#[expect]` / `#[allow]` surfaced by Phase 0c linter

**Goal**: clean up the workspace's `#[expect]` / `#[allow]` attribute surface — either remove sites OR add justified allowlist entries. The Phase 0c linter (`expect-allow-ban`) mechanically enumerates every site; this phase is the per-site human review and disposition.

**Why this is now post-Phase-0c (not pre-)**: per D-EXP.7, Phase 0c lands CI red. P-0 is the work to ratchet that red toward green by reviewing each surfaced attribute. Phase 0c did the enumeration tooling; P-0 does the disposition.

User's reservation (this session): existing `reason = "..."` strings on attrs may be LLM escape hatches that bypassed code review. They are NOT auto-extracted into allowlist baseline. Each site requires fresh human review.

**Inventory at Phase 0c landing time** (workspace-wide real attrs, after syn-based detection ignores comments + macro string literals):

- **Production `#[expect]`** (9 sites):
  - 8x `#[expect(unsafe_code, ...)]` — `src/runner.rs:794/806/1329/1584/1592`, `src/suite.rs:641`, `src/output/render.rs:1147/1158` (or close).
  - 1x `#[expect(clippy::unnecessary_wraps)]` — `src/output/pipe.rs:286`.
- **Production `#![expect]`** (crate/mod-level inner): 1x `#![expect(unsafe_code)]` at `src/output/pipe.rs:16`.
- **Production `#![allow]`** (crate-root inner): ~5 sites — `cargo-rudzio/src/generate.rs:1`, `cargo-rudzio/src/main.rs:7`, `migrate/src/lib.rs:11`, `src/runtime/embassy/runtime.rs:9`, `migrate/tests/golden.rs:2`. Each carries multi-lint allow + detailed reason.
- **Production `#[allow]`** (outer): ~1-4 sites in production code (after filtering out `quote!{}` macro emissions in `macro-internals/src/suite_codegen.rs` and `migrate/src/emit.rs` which are not real attrs in source position).
- **Test/fixture `#[expect]`** (~147 sites): mostly `clippy::unnecessary_wraps` from `rudzio::test` macro contract forcing `anyhow::Result<()>` return on trivial fixtures, plus `unsafe_code` RAII guards around `env::set_var`/`remove_var` (Rust 1.80+ unsafe), plus `clippy::too_many_lines` on template helpers, plus `clippy::panic` on deliberate panic fixtures.

**Design smells surfaced**:

1. **`rudzio::test` macro contract** forces `anyhow::Result<()>` return — trivial test fns trip `clippy::unnecessary_wraps`. ~50-80 of the test-site expects exist solely to suppress this. Long-term fix: macro accepts `()` returns. **Out of scope for current campaign; logged in `.opencode/issues.md`.**
2. **`env::set_var`/`env::remove_var` test guards** (Rust 1.80+ unsafe) duplicated across test files. Long-term fix: shared `cargo-rudzio-test-helpers` wrapper or `temp-env` dep. **Out of scope; logged in `.opencode/issues.md`.**
3. **8 `unsafe_code` production sites** await user's consolidation crate (D-EXP.5).

**Actions**:

1. **Run linter**: `just check-lints` (or directly: `cargo run --manifest-path linters/Cargo.toml -p expect-allow-ban`). Output: `linters/expect-allow-ban-report.txt` with every attr by `file:line kind=<expect|allow> lints=[...] reason=<text|MISSING>`.

2. **Per site (human review, this session)**:
   - Read the surrounding code. Decide:
     - **REMOVE**: code can be refactored to not need the suppression (best outcome).
     - **ALLOWLIST**: legitimate need; write a fresh, vetted `reason` string in `linters/expect-allow-ban/allowlist.toml`. Do NOT copy existing reason text without re-reviewing.
     - **DEFER**: site can't be addressed in this campaign (e.g., `unsafe_code` sites pending consolidation crate per D-EXP.5). Allowlist with reason `"deferred: <reason>; tracked in .opencode/issues.md"`.

3. **Re-run linter**: confirm CI step now passes (or only intentional deferrals remain, each allowlisted).

**P-0 verification gates**:
- `just check-lints` exits 0 for the `expect-allow-ban` step (or only deferred entries remain, each allowlisted).
- Workspace still compiles + tests pass.
- No `unsafe` block uncovered by a removed `expect` (would surface workspace `unsafe_code = deny` violation).
- Allowlist file contains exactly the number of entries linter expects (no orphans).

### P-1 — Default impl removal (REVISED v2 with concrete risk catalog)

**Goal**: zero `impl Default for T` AND zero `#[derive(Default)]` in workspace + lint rule preventing future ones.

**Concrete current state** (from agent audit):
- 1 manual `impl Default` (`cargo-rudzio/src/generate.rs:950 WorkspaceDepSpec`).
- ~16 `#[derive(Default)]` sites.
- 5+ struct-update consumer patterns: `..manifest::Edits::default()` (`migrate/src/run.rs:203`), `..ParsedArgs::default()` (`src/config.rs:1331`), `Self::default()` calls (`migrate/src/test_context.rs:169`, `src/output/capture_guard.rs:93`).
- External `Default` requirements (e.g. `token::Brace::default()` from syn, `LocalOptions::default()` from tokio) are unaffected.

**Actions**:

1. **(Agent — Explore)**: enumerate every `impl Default for T` + every `#[derive(Default)]` + every `..X::default()` struct-update + every `T::default()` call (filter out external types like `syn::*`, `tokio::*`, std types) → `.opencode/p1-default-audit.md`.

2. **(Human, this session)**: review. Per type with Default:
   - List every caller (existing report from agent).
   - Decide replacement: existing `new()`/`builder`, or new `fn new() -> anyhow::Result<Self>`.
   - For struct-update consumers (`..X::default()`): pick replacement value per field (or invent a `fn empty() -> Self` if zero-init is meaningful).

3. **(Human, this session)**: per type, edit (in dependency order — leaves first):
   - Remove `impl Default` / `#[derive(Default)]`.
   - Add replacement constructor.
   - Update each caller.
   - Run `cargo check` after each type's batch.

4. **(After all Default removed)**: add a new linter `linters/no-default-impls/` (or extend `non-result-fns` with a Default-impl detector) that fails CI on any future `impl Default for T` or `#[derive(Default)]`. Excludes std/external trait crates.

**P-1 verification gates**:
- Zero workspace-internal `impl Default for T` and `#[derive(Default)]`.
- Linter rule active.
- All tests pass.

### P-2 — DELETED (v2)

Per audit, the workspace has zero manual `impl Clone for T`. The original P-2 had nothing to enumerate. Removed.

If the workspace later acquires manual `impl Clone for T` (post-cleanup), the linter can be extended to flag them at that time.

### P-3 — Iterator handling (REVISED v2 to minimal scope)

**Goal**: audit the two `impl Iterator<Item=...>` *return positions*; decide per-site whether the Item should be retyped.

**Concrete current state**: zero `impl Iterator for T`. Two functions return `impl Iterator<Item = X>`:
- `src/output/render.rs:1251` (verify exact signature + Item type during execution)
- `cargo-rudzio/src/generate.rs:2686` (same)

**Actions**:

1. **(Human, this session)**: read both functions. Determine:
   - What is the Item type today?
   - Could the iterator emit error-bearing states the consumer should handle (parse failures, IO failures)?
   - If yes: retype Item to `Result<X, anyhow::Error>`; update consumers.
   - If no: document why the bare Item is safe; no retype needed.

2. **(Linter)**: ensure these two sites are visible to whatever metric tracks Iterator-shaped non-Result paths. May not need a separate gate.

**P-3 verification gates**:
- Each of the 2 sites either: retyped + consumers updated, OR documented as safe.
- All tests pass.

### P-4 — Linter extension (REVISED v2 with expanded scope)

**Goal**: extend `linters/non-result-fns/` to handle:
- Trait-locked impl skip list (Display, Debug, PartialEq, PartialOrd, Ord, Eq, Hash, Send, Sync, Drop, AsRef, AsMut, From, Into, etc.).
- Skip `fn next` inside `impl Iterator for T` (vacuous per D-2 v2 but still defensive).
- Skip fns listed in an allowlist file (D-16).
- Count `TODO(rs-cleanup)` markers; report alongside hazard count.
- Count `#[expect(...)]` markers (per P-0); track as monotonic-decrease gate.
- Eventually flip from `hazards = 0` to `non-Result count = 0`.

**Allowlist file format** (`linters/non-result-fns/allowlist.toml`):

```toml
# Each entry: fully-qualified fn path + written justification.
# Reviewed by humans; cargo-rudzio team owns this list.

[[allow]]
path = "rudzio::common::time::fmt_duration"
reason = "Pure formatter; clamps to display max via saturating_sub; no propagation path exists. Reviewed YYYY-MM-DD."

[[allow]]
path = "..."
reason = "..."
```

**Actions**:

1. **(Agent worktree, write-only)**: extend `linters/non-result-fns/src/main.rs`:
   - Trait-impl detection (parse `impl Trait for Type`; check trait against skip list).
   - Allowlist file parser + apply.
   - TODO counter.
   - `#[expect]` counter (per-site report, possibly grouped by lint name).
   - Add CLI flags: `--gate-todos` to fail when TODO count > 0; `--gate-expects N` to fail when expect count > N.

2. **(Human, this session)**: build + verify the extension by running it. Compare counts against `BASELINE.md`. Update `BASELINE.md` with the new metrics (TODO count, expect count by lint).

3. **(Human, this session)**: extend `Justfile` `check-lints` to invoke the new gates after Phase 1 completes (initially the new gates are inactive; activated phase-by-phase).

**P-4 verification gates**:
- Extension compiles + runs.
- Reports stable counts.
- Allowlist parsing works (test with a synthetic allowlist entry).

## Main work (M)

### M-1 — Hybrid sig transformer tool (REVISED v2 with explicit edge cases)

**Goal**: deterministic, dry-run-capable tool that transforms one Rust file:
- Changes fn signatures `-> T` to `-> anyhow::Result<T>`.
- Wraps bodies in `Ok(...)`.
- Inserts TODO/FIXME markers.

**Crate location**: `linters/rs-result-sig-rewrite/` (sub-workspace member).

**CLI surface**:

```
rs-result-sig-rewrite --file <absolute-file-path> [--dry-run] [--write]
```

- `--file` mandatory; absolute path.
- Default mode is `--dry-run` (print diff, no write).
- `--write` mode requires absolute path + an explicit confirm flag (no accidental overwrite).
- Exit codes: 0 = clean, 1 = no changes, 2 = parse error, 3 = unsupported case found.

**Edge case policy** (each must be documented in tool's source + tested with a synthetic input):

| Case | Tool behavior |
|---|---|
| Trait method impl (e.g. `impl Display for X`) | Skip. Emit log `skipping Display::fmt at file:line`. |
| Iterator impl `fn next` | Skip. (Per D-2 v2 — vacuous in this workspace but defensive.) |
| `async fn name() -> T` | Transform: `-> T` to `-> anyhow::Result<T>`. Body: same wrap-in-Ok logic. (Async fn return is syntactically `T`; semantically `impl Future<Output=T>` but syn sees the declared return.) |
| `const fn name() -> T` | **Skip with warning**. `anyhow::Result<T>` cannot be constructed in const context (anyhow::Error has non-const constructors). Emit log + exit code 3 if this is unexpected. |
| Generic fn with `where` clause | Transform sig as usual. Where clauses preserved unchanged. |
| `fn name() -> impl Trait` | Transform: `-> impl Trait` to `-> anyhow::Result<impl Trait>`. Compiles only if `impl Trait` is non-recursive in returns. If the body doesn't wrap correctly, tool emits FIXME marker. |
| `fn name() -> !` (never type) | Skip. The fn never returns; wrapping in Result is meaningless. |
| Fn body uses `?` already | Existing `?` works; tool inserts `Ok(...)` around tail expression. |
| Fn body contains macro invocation | Tool processes only what syn sees (the macro call itself, not its expansion). Macro-generated fns are invisible to syn — not transformed. |
| `proc-macro` / `macro_rules!` definitions | Skip the macro bodies entirely (syn does parse `macro_rules!` items but the body is opaque). |
| Fn returning `()` (no explicit return type) | Transform: insert `-> anyhow::Result<()>`. Body: append `Ok(())`. |
| `return X;` statements | Rewrite to `return Ok(X);`. |
| Tail expression with side effects (e.g. `foo();` followed by no return) | Insert `Ok(())` at end. |
| Hazard sites detected by reused non-result-fns visitor | Insert `// TODO(rs-cleanup): <hazard kind> at line N — review and replace with Result-propagating equivalent` |
| Intra-file call sites of converted fns | Insert `// FIXME(rs-cleanup): callee returns Result, add ?` BEFORE the call site (one line above), or insert `?` if call site is simple enough. Tool's default: insert FIXME; humans add `?` during review. |
| Cross-file call sites | NOT touched by the tool. Human grep + update. |

**Skip list for trait-impl fns** (passed to tool as CLI arg or read from a config):

```
std::fmt::Display, std::fmt::Debug, std::fmt::Binary, std::fmt::Octal, std::fmt::LowerHex, std::fmt::UpperHex,
std::cmp::PartialEq, std::cmp::Eq, std::cmp::PartialOrd, std::cmp::Ord,
std::hash::Hash,
std::ops::Drop,
std::marker::{Send, Sync, Unpin, Sized, Copy},
std::convert::{AsRef, AsMut, From, Into, TryFrom, TryInto},
std::iter::{Iterator, IntoIterator, FromIterator, DoubleEndedIterator, ExactSizeIterator},
std::clone::Clone, // Pending derive replacement per D-1; once gone, this entry can be removed
```

Trait paths use textual match (right-most segment): `Display`, `Debug`, `Clone`, etc. The tool tolerates fully-qualified vs. partially-qualified.

**Actions**:

1. **(Agent worktree, write-only)**: create the crate. Implement per above. Include 5-10 synthetic input/output samples in `linters/rs-result-sig-rewrite/tests/` for each edge case category.

2. **(Human, this session)**: build via `cargo build --manifest-path linters/Cargo.toml -p rs-result-sig-rewrite`. Run dry-run against `src/common/time.rs` (smallest hot-spot, 9 hazards, 1 fn). Inspect diff. Iterate until correct.

**M-1 verification gates**:
- Tool builds clean.
- Dry-run on `src/common/time.rs` produces a reasonable diff matching expected transformation.
- Synthetic edge-case tests pass.

### M-2 — Per-module PRs, hot-spot order (REVISED v2 with cross-crate map)

**Order by hazard density** (from `linters/BASELINE.md`):

| Order | File | Hazards | Crate | Cross-crate consumers |
|---|---|---:|---|---|
| 1 | `src/output/render.rs` | 71 | rudzio | None (internal API) |
| 2 | `src/bench/report.rs` | 61 | rudzio | None (internal API) |
| 3 | `migrate/src/emit.rs` | 27 | rudzio-migrate | None (internal to migrate) |
| 4 | `src/bench/progress_snapshot.rs` | 25 | rudzio | None |
| 5 | `src/runner.rs` | 25 | rudzio | None (internal API) |
| 6 | `cargo-rudzio/src/generate.rs` | 21 | cargo-rudzio | None |
| 7 | `migrate/src/rewrite.rs` | 21 | rudzio-migrate | None |
| 8 | `src/config.rs` | 14 | rudzio | None |
| 9 | `src/shuffle.rs` | 10 | rudzio | None |
| 10 | `src/common/time.rs` | 9 | rudzio | None |
| 11+ | remaining files | < 9 | mixed | spot-check per file |

**Cross-crate dependency graph** (from Cargo.toml audit):
- `cargo-rudzio` depends on `rudzio-migrate`, `rudzio`, `cargo-rudzio-test-helpers`
- `cargo-rudzio-test-helpers` depends on `rudzio`
- `migrate` depends on `rudzio`
- `macro` depends on `macro-internals`
- `macro-internals` depends on `rudzio`

Per-file rule: when changing a function signature in `src/` (rudzio crate), check whether the function is `pub` and used cross-crate. If yes, the PR also updates the 1-5 downstream callers (count depends on the function).

Specifically: `rudzio` public API converted to Result → ripples to ALL of `cargo-rudzio`, `cargo-rudzio-test-helpers`, `migrate`, `macro-internals`. Plan estimate: 1-3 cross-crate caller updates per PR; rare PRs may have more.

**Per-PR shape**:

1. (Human, this session) — run M-1 tool against the file in `--dry-run`. Review diff.
2. (Human, this session) — run M-1 tool with `--write`.
3. (Human, this session) — fix `FIXME(rs-cleanup)` markers within the file (add `?`).
4. (Human, this session) — grep cross-file for callers, update each with `?` + `.with_context(...)`.
5. (Human, this session) — `cargo check` + `cargo clippy` + run affected tests (background if >120s).
6. (Human, this session) — commit + push.

**Smaller hot-spot files (items 9, 10, 11+)** may be delegated to agents per D-12/D-13/D-14:
- Agent gets tool output as starting point.
- Agent updates TODOs/FIXMEs + same-file callers.
- Agent does NOT run cargo (D-13).
- Human runs verification + commits.

### M-3 — Cosmetic pass for remaining ~468 non-hazard fns

Same per-file workflow as M-2. Group by directory.

## Phase 2 — TODO sweep (REVISED v2 escape hatch)

**Goal**: zero `TODO(rs-cleanup)` markers in workspace.

Per TODO decide:
- **Real hazard** (overflow not expected, propagate the error): replace masking op with `checked_*?` / `.ok_or_else(...)?` + `.with_context(|| "...")` for provenance.
- **Intentional clamping** (display width, layout): replace with explicit code that makes intent obvious — e.g. `value.min(MAX)` instead of `value.saturating_add(...)`. Add brief inline comment explaining intent. NO `#[expect]` per D-16.

**For fns that should genuinely stay non-Result** (rare): add entry to `linters/non-result-fns/allowlist.toml` with written justification. Each allowlist entry requires human review.

**Quantitative criteria for hazard vs intent**: no formal rule. Heuristic guidance:
- If the value being computed is bounded by a known external invariant (e.g. terminal width ≤ u16::MAX), saturating to that bound is intent.
- If the input could exceed the output's bound under any realistic scenario (user input, file sizes, time deltas), the saturation is hiding an error path.
- When in doubt: lean toward propagation (Result).

## Verification gates per phase

| Phase | Required passing |
|---|---|
| End of Phase 0 (0a + 0b) | All proactive tests green incl. new bug_17/bug_18 cases; dogfood unchanged; clippy clean; pushed to main. |
| End of P-0 | `#[expect]` count audit complete; reductions committed; tests pass; clippy clean. |
| End of P-1 | Zero workspace-internal `impl Default for T` and `#[derive(Default)]`. Default-lint rule active. Tests pass. |
| End of P-3 | Each of the 2 Iterator return-position sites either retyped + consumers updated, OR documented as safe. |
| End of P-4 | Linter extended; new gates configurable (initially inactive); allowlist.toml schema in place. |
| End of M-1 | Tool built; dry-runs clean on `src/common/time.rs`. |
| End of M-2 | All hot-spot files: hazards = 0 (per linter). Tests green. |
| End of M-3 | Workspace non-Result fn count at zero (excluding allowlist entries). Tests green. |
| End of Phase 2 | `TODO(rs-cleanup)` count at zero. Linter all gates green. `just ci` passes. |

## Verification gate execution policy (NEW v2)

Each cargo command in a verification step is evaluated against the 120s shell timeout:

| Command | Typical duration | Execution policy |
|---|---|---|
| `cargo check -p <crate>` | < 60s usually | Foreground Bash call. |
| `cargo clippy -p <crate> -- -D warnings` | 60-180s | Foreground if ≤ 120s expected; background via setsid + log polling otherwise. |
| `cargo run -p cargo-rudzio -- test <spec>` (single small spec) | 10-30s | Foreground. |
| `cargo run -p cargo-rudzio -- test <large_spec>` (e.g. `generate_spec`'s 632 tests, full workspace `test`) | 50-1200s | Background via `Bash` `run_in_background: true`. Poll `/tmp/<spec>.log` periodically. |
| Multi-crate `cargo build --workspace` | 200-600s | Background. |

For backgrounded runs in this session:
```bash
# launch
cargo run -p cargo-rudzio -- test generate_spec > /tmp/generate_spec.log 2>&1 &
# poll via Bash run_in_background OR manually with `Read /tmp/generate_spec.log`
# detect completion via final `test result:` line + exit code in log
```

## Risks (REVISED v2)

1. **Iterator scope: actually trivial now.** Two return-position sites. No workspace-wide ripple. Risk demoted from "biggest" to "negligible".
2. **Default removal callers**: 5+ struct-update consumers identified. Each needs explicit per-field replacement. Manageable.
3. **`#[expect]` audit may surface design smells beyond scope**: the audit may reveal that some `#[expect(unsafe_code)]` blocks need refactoring (e.g., replace with a safe API). Track in P-0 report; defer non-cleanup-blocking ones.
4. **Some `saturating_*` is intentional.** Phase 2 distinguishes hazard from intent via heuristic + allowlist. Expect ~30-50% of masking-arith TODOs resolve as "intentional + explicit clamp via `.min(...)`".
5. **Tool quality**: dry-run mode mandatory. No production write without human review.
6. **Downstream consumers see breaking API changes**: pre-1.0 sanctioned. CHANGELOG entry per minor bump.
7. **CI red on `check-lints` during campaign**: hazard count drops PR by PR. Gate flips green only after Phase 2.
8. **Verification cycle bottleneck (50-80 cycles in this session)**: real throughput ceiling. Backgrounded runs mitigate the per-run cost but not the count. Pipelining (next PR prepared while previous tests run) recommended.
9. **Worktree target dir accumulation**: 12+ worktrees × 5-40 GB each = up to 480 GB worst case on /var/mnt/data-cache. Cleanup policy below.

## Operational fixes (NEW v2)

### Worktree target directory setup

In this session, BEFORE any agent launches:

```bash
mkdir -p /var/mnt/data-cache/cargo/worktree-targets
```

Each worktree-isolated agent gets a unique target dir under this path. Naming: `<worktree-target-dir>/<task-id>/` where `<task-id>` is the agent's task ID (e.g., `m1-tool-build`, `p4-linter-ext`).

Per-agent brief:
- Set `CARGO_TARGET_DIR=/var/mnt/data-cache/cargo/worktree-targets/<task-id>/`.
- Do NOT source `.opencode/cargo-env.sh` (it would clobber the per-worktree dir).
- Do NOT run any cargo command anyway (D-13).

### Worktree cleanup

After agent completion (in this session):

```bash
# If accepting agent output:
git worktree remove <worktree-path>
# Worktree's target dir (if any) is preserved at /var/mnt/data-cache/cargo/worktree-targets/<task-id>/
# Move to archive or delete:
rm -rf /var/mnt/data-cache/cargo/worktree-targets/<task-id>/

# If rejecting agent output:
git worktree remove --force <worktree-path>
rm -rf /var/mnt/data-cache/cargo/worktree-targets/<task-id>/
```

End-of-session cleanup script (if needed): walk `/var/mnt/data-cache/cargo/worktree-targets/` and remove dirs whose `<task-id>` is not in an active-agents list.

### Tool binary absolute path for agents

After M-1 builds the tool in this session:
- Absolute path: `/var/home/fathomio/Projects/PorfiryPetrovich-ai/feat/platform-backend/shim/Projects/mykytanikitenko/rudzio/main/linters/target/debug/rs-result-sig-rewrite` (verify exact path after build).
- Each agent brief that uses the tool includes this absolute path AND absolute paths to the target file(s).
- Tool's CLI accepts `--file <abs-path>` per M-1 spec.

### `.opencode/` write target for agents

Agent briefs MUST instruct agents to write progress files to absolute paths in the MAIN worktree:
- `/var/home/fathomio/Projects/PorfiryPetrovich-ai/feat/platform-backend/shim/Projects/mykytanikitenko/rudzio/main/.opencode/agent-progress-<task-id>.md`

NOT to the worktree-local `.opencode/`. Agents working in worktrees can read/write to main `.opencode/` because the worktree mounts the same filesystem.

### `cargo-env.sh` policy for agents

Agents do NOT source `.opencode/cargo-env.sh`. Per-task brief sets only the env vars the agent needs (typically: nothing, since agents don't run cargo).

If an agent needs `CARGO_NET_OFFLINE=true` for some reason (e.g., tool invocation indirectly calls cargo through a build script — unlikely but possible), the brief sets it explicitly.

This session continues to source `cargo-env.sh` as before.

### Verification command 120s timeout policy

Per "Verification gate execution policy" above. Commands likely >120s use `Bash` with `run_in_background: true`. Poll logs.

### Rollback procedure (NEW v2)

If a PR mid-campaign needs to be reverted due to a critical bug:

1. Identify the offending commit SHA: `git log --oneline | head -20`.
2. Create a revert commit: `git revert <SHA>` (preserves history) OR `git reset --hard <prior-SHA>` + force-push (DESTRUCTIVE — only with user approval).
3. Per project rules: NEVER `--force` to main without explicit user direction.
4. Default: use `git revert <SHA>` then push. Re-run the failed verification gates on the revert commit.
5. Document the revert in a CHANGELOG entry.

If multiple PRs need to be reverted in sequence: revert in reverse order (newest first) to minimize merge conflicts.

### CHANGELOG.md entries (NEW v2)

Entry templates:

**Phase 0 entry (v0.1.x patch bump or v0.2.0):**

```markdown
## [v0.2.0] - 2026-MM-DD

### Fixed
- Bug #17: `[[bin]] name = "..."` with mismatched parent dir name no
  longer silently drops bin-source `#[rudzio::test]` markers. The
  directory-style detection now keys on `path` ending in `main.rs`
  with a non-shared parent dir, not on `name` matching parent dir name.
- Bug #18: aggregator cache subdir now includes a per-cargo-rudzio-
  rebuild stamp hash. Old cargo-rudzio versions' aggregator dirs no
  longer race with new versions; manual GC via
  `rm -rf <target>/rudzio-auto-runner-*` documented.

### Changed
- ACCEPTED REGRESSION (D-17): non-Cargo-idiomatic
  `[[bin]] name = "foo", path = "src/foo/foo.rs"` (parent dir matches
  bin name, but file_name is not "main.rs") no longer walks the parent
  dir for rudzio test markers. Use `path = "src/foo/main.rs"` for
  directory-style support.

### Migration
- Existing aggregator dirs from prior cargo-rudzio versions can be
  cleaned with: `rm -rf <consumer-workspace>/target/rudzio-auto-runner-*`
```

**Phase 1+ campaign entry (v0.2.x or v0.3.0):**

Drafted at end of campaign with full list of API-breaking signature changes.

## Agent delegation summary (REVISED v2)

| Task | Delegate? | Mechanism | Cargo allowed? |
|---|---|---|---|
| P-0 audit | Yes (Explore) | Read-only enumeration | No (D-13) |
| P-1 audit | Yes (Explore) | Read-only enumeration | No |
| P-3 audit / decision | Manual (only 2 sites) | Human reads + decides | — |
| P-4 linter ext | Yes (general worktree) | Write code only; human verifies | No |
| M-1 tool build | Yes (general worktree) | Write code only; human verifies + dry-runs | No |
| Default removal — per type | Manual | Per-type caller context | — |
| M-2 hot-spot files (1-8) | Manual + tool-assisted | Tool runs in this session; human reviews + caller updates | — |
| M-2 smaller files (9, 10, 11+ with ≤10 hazards) | Yes (narrow brief) | Tool output + agent updates TODOs + same-file callers | No (D-13); binary calls OK (D-14) |
| M-3 cosmetic per file | Yes (narrow brief) | Same as M-2 smaller | No; binary calls OK |
| P-3 Iterator audit action | Manual | 2 sites; human inspects | — |
| TODO sweep — pattern-based | Yes (narrow brief) | Single pattern at a time | No; binary calls OK |
| TODO sweep — judgment cases | Manual | Per-site semantic decision | — |
| `git push` / commit creation | Human only | — | — |
| CI gate threshold tightening | Human only | — | — |

## Monitoring agents (REVISED v2)

Per D-11/D-12/D-13:
- Agents write progress files to `/var/home/.../main/.opencode/agent-progress-<task-id>.md` (absolute path; not worktree-local).
- Agents NEVER run cargo.
- This session monitors by reading the absolute-path progress file + worktree diff inspection.

Wave 1 launch (per D-10 v2): only P-0 audit + P-1 audit are parallel-eligible (both Explore agents). P-3 is manual. P-4 launches after P-0/P-1 reports inform the skip-list and counters. M-1 launches in parallel with P-4 if briefs are independent.

Failure detection: `.opencode/agent-progress-<task-id>.md` mtime stale OR missing → discard worktree (D-11 strict).

## Disk budget (NEW v2)

- `/var/mnt/data-cache/cargo/target/` (main session): ~43 GB currently.
- Per worktree target: typically 5-40 GB after a fresh build.
- Expected concurrent worktrees: 2-3 typical, peak 5 during wave 1.
- Estimated additional disk: 50-200 GB peak.
- `/var/mnt/data-cache/` free: 520 GB.
- **Budget**: comfortable. Sweep stale worktree dirs at end of each phase.

---

## Open items requiring user input

| Item | When | Decision needed |
|---|---|---|
| Green light for Phase 0 execution | After this PLAN.md commits | "go" command |
| Green light for Phase 1+ pre-work | After Phase 0 commits + pushes | Confirm proceeding |
| P-0 audit review (`#[expect]` policy per category) | After P-0 audit completes | Per-category remove vs keep |
| P-1 audit review | After P-1 audit completes | Per-type Default replacement strategy |
| M-1 tool dry-run review | After M-1 tool built | Confirm diff on sample file |
| Each M-2 PR | Per hot-spot file | Diff review before commit |
| Each Phase 2 sweep PR | Per TODO group | Diff review before commit |
| Linter gate tightening | At end of M-3 | Confirm flip from `hazards = 0` to `non-Result count = 0` |
| Linter gate flip from off to active for `#[expect]` count | At end of P-0 | Confirm `expect count` becomes a monotonic-decrease gate |

---

## Execution principles

- **No counter-productive escape hatches**. "ASAP" doesn't license shortcuts that mask bugs.
- **Test before fix**. Bug #17 + Bug #18 reproducer tests written and failing before fixes land.
- **Loud failures**. No silent fallthroughs. Mismatch → explicit. Unmounted markers → hard error. Hazards → CI red.
- **Single source of truth for cargo invocations**. This session only.
- **Write-only agents**. Agents don't verify. Humans verify in this session.
- **Progress files strictly enforced**. Missing → discard.
- **Worktree isolation**. Per D-15 v2, target/ on `/var/mnt/data-cache/cargo/worktree-targets/`.
- **Commit only on full verification**. All proactive specs + dogfood + clippy green before any commit.
- **Push only after commit verified locally**. No `--force` to main; no `--no-verify`.

---

## Glossary

- **Phase 0a**: Bug #17 fix (heuristic correction + 3 new tests in bug_16 spec).
- **Phase 0b**: Bug #18 fix (subdir-hash + build.rs stamp + 1 new test in NEW bug_18 spec).
- **Phase 0**: combined 0a + 0b commit + push.
- **Pre-work**: P-0 (`#[expect]` audit) + P-1 (Default removal) + P-3 (Iterator return-position audit, 2 sites) + P-4 (linter extension). P-2 DELETED.
- **Main work (M)**: M-1 tool build + M-2 per-module hot-spot conversions + M-3 cosmetic pass.
- **Phase 2**: TODO sweep — addressing every `TODO(rs-cleanup)` marker.
- **Hazard**: `unwrap()`/`expect()`/silencing combinator/arithmetic op outside checked-*/indexing/panic macro/numeric `as` cast/masking arithmetic (saturating_*/wrapping_*).
- **Trait-locked impl**: trait method impl whose signature is fixed by std (Display::fmt, Debug::fmt, PartialEq::eq, Hash::hash, etc.). Excluded from cleanup. After P-1: Default removed entirely.
- **Build stamp**: `<CARGO_PKG_VERSION>-<epoch_seconds>` captured by `cargo-rudzio/build.rs`.
- **Aggregator subdir name (v2)**: `rudzio-auto-runner-<workspace_hash:016x>-<stamp_hash:016x>`.
- **Directory-style bin**: `[[bin]] path = ".../main.rs"` where parent dir is not the lib's parent. Owns its source tree; recursive walk valid.
- **Flat-style bin**: `[[bin]] path = ".../<name>.rs"` (non-main.rs filename) OR `src/main.rs` with sibling `src/lib.rs`. Single-file scope.
- **`?`-propagation**: `let x = fn()?;`.
- **`.with_context(|| "...")`**: anyhow's error provenance attachment.
- **Allowlist**: `linters/non-result-fns/allowlist.toml` — file-level documented exceptions to the non-Result gate; replaces `#[expect]` as the escape hatch per D-16.

---

**End of plan v2.** Awaiting green light to begin Phase 0 execution.
