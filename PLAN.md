# PLAN.md — Result-everywhere conversion

**Goal:** Every function in the codebase returns `Result`, propagates errors
with context, and reports the full error chain at the boundaries. Error
handling > backward compatibility — breaking current logic is acceptable.

**Active branch:** `feat/result-everywhere` (intermediate commits may be RED;
only the FINAL merge commit must be green). `main` stays green.

---

## Locked decisions (user)

1. **Every function returns `Result`.** No function left non-`Result` except
   the irreducible locked boundaries (§ Boundaries).
2. **Error type per layer:**
   - Library / runtime / tool crates (`rudzio` core, `migrate`, `cargo-rudzio`,
     `cargo-rudzio-test-helpers`): **`anyhow::Result`**, errors carry
     `.context(...)`.
   - Proc-macro layer (`macro`, `macro-internals`): **`syn::Result`** (becomes
     `compile_error!` — the correct proc-macro error domain). Confirmed "fine".
3. **Lint policy (no suppression-to-hide):**
   - `clippy::unnecessary_wraps` = **allow** — design-aligned: a total fn
     intentionally returns `Ok(value)` so callers uniformly `?`. This is a
     deliberate config choice, the only one that lets a pure fn return
     `Result`. Already set in `Cargo.toml`.
   - `clippy::missing_errors_doc` = **forbid** (group default) — every
     `pub fn -> Result` MUST carry a hand-written `# Errors` section.
   - `unwrap_used` / `expect_used` / `panic` / `indexing_slicing` /
     `arithmetic_side_effects` stay **forbid** — propagate, don't panic/swallow.
4. **On error: report the full chain and return/abort.** Reporter / IO failures
   **abort the run** (propagate to top, `eprintln!("{err:?}")`, non-zero exit).
5. **Final-commit-green only.** Work proceeds on the branch; merge when the
   whole workspace is green + tests pass.

## Boundaries (irreducible — cannot return `Result`)

The trait signature is fixed by an external crate or the executor model.
Inside these methods, on internal `Err`: **report the full chain to stderr,
then return the trait-required value** (user decision: report + return).

- **Runtime executor trait** (`src/runtime/rt.rs` + 9 adapters + `join_error.rs`):
  `block_on`/`spawn`/`spawn_blocking`/`spawn_local` bound to the async
  ecosystem + `Future`. Locked. (Already reverted to non-`Result` on branch.)
- **std/syn locked traits:** `Display::fmt`, `Debug::fmt` (→ `fmt::Result`),
  `Drop::drop` (→ `()`), `Iterator::next` (→ `Option`), `From::from` (→ `Self`),
  `VisitMut::visit_*` (syn, ×3 in `migrate/src/rewrite.rs`).

Everything else converts.

---

## What I need to convert all functions to `Result`

### Prerequisite tooling — `rs-result-sig-rewrite` (M-1) enhancements

The per-file manual cascade is the bottleneck (344 core-lib errors after a bulk
transform are mostly missing `?`). To convert the whole workspace without
weeks of hand-editing, M-1 needs:

- **T-1 `# Errors` doc-stub insertion.** With `missing_errors_doc` forbidden,
  every `pub fn` that becomes `-> Result` needs a `# Errors` section or the
  build fails. M-1 must insert a stub (`/// # Errors\n/// Returns `Err` if …`)
  above each converted `pub fn`. WITHOUT this, the conversion cannot pass
  clippy. **Highest-priority tooling gap.**
- **T-2 Automatic `?` at intra-file call sites.** M-1 currently only emits
  `// FIXME(rs-cleanup): add ?` comments. It must actually insert `?` on calls
  to fns it converted (assignments `let x = f(..)?;`, `if !f(..)?`, method
  chains `f(..)?.method()`), since the enclosing fns are now `Result`. This is
  the bulk of the 344-error cascade.
- **T-3 `syn::Result` mode** (`--error-type syn`). For the macro layer: emit
  `syn::Result<T>` instead of `anyhow::Result<T>`, and `?` at `quote!`
  interpolation sites (`let x = build(..)?; quote!{ #x }`).
- **T-4 `anyhow::Result`→`Result` normalization** when the file already
  imports `Result` (avoids `unused_qualifications` forbid). Currently a manual
  `sed` step.
- **T-5 Locked-trait skip.** Extend skip handling so `VisitMut`, `Drop`,
  `Display`, `Debug`, `Iterator`, `From` impl methods are never converted
  (TRAIT_SKIP_LIST already covers std ones; add `VisitMut`). `--pub-only`
  already skips all trait impls — but private inherent fns still need
  converting, so a dedicated skip is cleaner than `--pub-only`.
- **T-6 Drop `#[must_use]`** on fns being converted to `Result` (`Result` is
  already `#[must_use]`; clippy `double_must_use` fires otherwise).

Build + test each enhancement against the existing 18 M-1 snapshot tests; add
new fixtures for T-1/T-2/T-3.

### Conversion workstreams (after tooling)

| WS | Scope | Notes |
|----|-------|-------|
| A | Lint config | `unnecessary_wraps` allow (done); `missing_errors_doc` forbid (done — reverted). |
| B | `rudzio` core lib — leaf modules | `common/time.rs`, `output/color.rs`, `output/init.rs`, `parallelism.rs`, `context/**`. Pure `?` cascade after T-1/T-2. |
| C | `rudzio` core lib — `config.rs` | Tolerant parser; convert + `# Errors`; the "never errors today" fns return `Ok` (unnecessary_wraps allowed). |
| D | `rudzio` core lib — `bench/report.rs`, `output/render.rs` | Formatters; `?`-cascade. Renderer write paths already partly `Result` (`split_lines`). |
| E | **Reporter keystone** | `Reporter` trait (`src/suite.rs`) + `ModeReporter` impl (`runner.rs`) → `Result`; update `macro-internals/src/suite_codegen.rs` `quote!` templates so the generated suite-runner propagates the reporter `Result` and **aborts the run** with a full-chain diagnostic + non-zero exit. Coupled unit; do as one change. |
| F | `rudzio` core lib — `runner.rs` dispatch | Remaining after E. Thread `Result` through dispatch to `run() -> ExitCode`. |
| G | Boundary bodies | Locked-trait methods (§ Boundaries): on `Err`, `eprintln!("{err:?}")` + return trait value. Runtime stays reverted. |
| H | `migrate` crate | `emit.rs` (pattern proven on `main` `37dd260`), `rewrite.rs` (VisitMut walls — skip those methods), `discovery.rs`/`detect.rs`/`backup.rs`/`run.rs`. |
| I | `cargo-rudzio` + `cargo-rudzio-test-helpers` | `generate.rs`, `cli.rs`, lib. |
| J | Macro layer (`syn::Result`) | `macro-internals` pure-codegen builders → `syn::Result` (T-3); `?` at quote! sites. `macro` proc-macro entry points already `syn::Result`. |
| K | Top-level | `run() -> ExitCode` / `main`: collect propagated errors, `eprintln!("{err:?}")` full chain, non-zero exit. |
| L | `# Errors` docs | Every `pub fn -> Result` (T-1 stubs + manual refinement of the "Returns Err if…" text). |

### Verification (final, before merge)

- `cargo clippy --workspace --all-targets --no-deps -- -D warnings` clean
  (with `missing_errors_doc` forbid → all `# Errors` present; `unnecessary_wraps`
  allowed).
- `cargo run -p cargo-rudzio -- test -- --threads-parallel-hardlimit=none`
  (full suite, all 9 runtimes — generated code runs here).
- `cargo test -p rudzio-migrate` (golden snapshots unchanged).
- Squash-merge `feat/result-everywhere` → `main`.

---

## Branch state (current)

- Lint allows: `unnecessary_wraps` allow; `missing_errors_doc` reverted to forbid.
- Bulk M-1 transform applied (492 fns → `Result`), `anyhow::Result`→`Result`
  normalized where imported.
- `src/runtime/**` reverted (locked executor boundary).
- `macro/**` + `macro-internals/**` reverted (need the `syn::Result` pass, WS-J).
- **344 compile errors remain, all in core `rudzio` lib** — the `?`-cascade
  (WS-B…F) + the Reporter keystone (WS-E). These are the immediate next work;
  T-1/T-2 tooling makes them tractable at scale.

Detailed continuation notes: `.opencode/issues.md` (2026-05-29 entries).

---

## Completed (historical ledger — context only)

All landed on `main` and pushed; release shipped.

- **Bugs #16/#17/#18/#19** fixed (`fa80fb2`, `261fa8a`, `820d257`, `1ebe772`).
- **Phase B** test-isolation framework (`IsolatedCargoSuite`/`IsolatedCargoTest`)
  + 8/10 spec migrations (`92151ff`…`5c7b359`).
- **Linters:** `expect-allow-ban` (Phase 0c) + P-0 allowlist (155 sites,
  `14934fe`); `no-default-impls` + P-1 `Default` removal (`1f60be7`);
  `non-result-fns` + P-4 extension (`3f7026a`).
- **P-3** `split_lines` Iterator retype (`7f6eea9`).
- **M-1** `rs-result-sig-rewrite` tool + `--pub-only` (`79754b0`, `8d67efa`).
- **M-2 (on `main`, pre-branch):** `shuffle.rs`, `bench/progress_snapshot.rs`,
  `migrate/emit.rs` converted with real error paths (`211c34f`, `bb34ab0`,
  `37dd260`). These demonstrate the per-file recipe.
- **Release `v0.1.2`** published to crates.io (all 5 crates); 0.1.0/0.1.1 yanked.
- **Bulk-transform reference:** branch `wip/m2-bulk-transform` (`5d4ab70`).
