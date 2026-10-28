//! Exact plain-mode runner-summary parsing and assertion helpers.
//!
//! Every test that spawns a child test binary (a standalone
//! `#[rudzio::main]` fixture, or a nested `cargo rudzio test`) and
//! inspects its output must assert the runner's *exact* result — not a
//! loose `contains("N passed")` or `>= N`. The fixture-isolation bug
//! (a fixture running the entire registry instead of its own tests)
//! slipped past loose assertions; these helpers make any leak or lost
//! test fail loudly.
//!
//! # Output format assumed
//!
//! The parser targets rudzio's **plain pretty** output (what a child
//! binary emits when stdout is a pipe / `CI` is set / `--output=plain`),
//! which is the mode every fixture and spec runs under via
//! `Command::output()` + `NO_COLOR=1`. The two shapes consumed are:
//!
//! * the summary line:
//!   `test result: {ok|FAILED}. {passed} passed; {failed} failed; \
//!   {panicked} panicked; {timed_out} timed out; {cancelled} cancelled; \
//!   {ignored} ignored; {teardown_failed} teardown failed; 0 measured; \
//!   {total} total; {filtered_out} filtered out; finished in {elapsed}`
//! * per-test result lines: `[{TAG}]    {qualified::name}    <runtime, elapsed>`
//!   where `TAG` ∈ `OK`, `FAIL`, `PANIC`, `TIMEOUT`, `CANCEL`,
//!   `IGNORE`, `BENCH`, `SETUP`, `HANG`. Suite lifecycle lines render
//!   as `setup <suite>` / `teardown <suite>` with the same tags and are
//!   excluded from the test-name set.

use std::collections::BTreeMap;
use std::process::Output;

use anyhow::{Context as _, Result};

/// Exact counters from a plain-mode `test result: …` summary line.
///
/// No `Default` impl exists: a caller must name every field, so a
/// counter that should be non-zero can never silently fall back to
/// zero via `..Default::default()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSummary {
    /// Tests that passed.
    pub passed: usize,
    /// Tests that failed (assertion / `Err`) or whose per-test context
    /// (`Suite::context`) returned `Err` — both land in `failed`.
    pub failed: usize,
    /// Tests that panicked.
    pub panicked: usize,
    /// Tests that exceeded their timeout.
    pub timed_out: usize,
    /// Tests cancelled before completion.
    pub cancelled: usize,
    /// Tests skipped via `#[ignore]`.
    pub ignored: usize,
    /// Combined suite-level + per-test teardown failures.
    pub teardown_failed: usize,
    /// Benchmark count — always `0` in plain mode (the runner prints
    /// the literal `0 measured`), kept for shape-completeness.
    pub measured: usize,
    /// Total tests considered, including ignored.
    pub total: usize,
    /// Tests filtered out by a CLI filter (always `0` when no filter
    /// is passed).
    pub filtered_out: usize,
}

impl RunSummary {
    /// Construct a plain-mode summary for the common case: no CLI test
    /// filter and no benchmarks, so `filtered_out` and `measured` are
    /// both zero. Every outcome counter is a named argument — no field
    /// can be silently omitted, and the two always-zero counters are
    /// set explicitly rather than defaulted.
    #[must_use]
    pub const fn plain(
        passed: usize,
        failed: usize,
        panicked: usize,
        timed_out: usize,
        cancelled: usize,
        ignored: usize,
        teardown_failed: usize,
        total: usize,
    ) -> Self {
        Self {
            passed,
            failed,
            panicked,
            timed_out,
            cancelled,
            ignored,
            teardown_failed,
            measured: 0,
            total,
            filtered_out: 0,
        }
    }
}

/// Merge a child's stdout and stderr into one lossy string — the
/// runner prints the summary and result lines to stdout, while some
/// diagnostics land on stderr, so exact-summary lookups scan the union.
#[must_use]
pub fn combined_output(output: &Output) -> String {
    let mut buf = String::from_utf8_lossy(&output.stdout).into_owned();
    buf.push_str(&String::from_utf8_lossy(&output.stderr));
    buf
}

/// Parse the plain-mode summary counters from `output`.
///
/// `output` is the combined (stdout + stderr) text of a child run.
/// The first `test result:` line is parsed.
///
/// # Errors
///
/// Fails loudly when no summary line is present, or when any counter
/// is missing or not a non-negative integer — never returns a
/// partially-filled summary.
pub fn parse_summary(output: &str) -> Result<RunSummary> {
    let tail = summary_tail(output)?;
    Ok(RunSummary {
        passed: counter(tail, "passed")?,
        failed: counter(tail, "failed")?,
        panicked: counter(tail, "panicked")?,
        timed_out: counter(tail, "timed out")?,
        cancelled: counter(tail, "cancelled")?,
        ignored: counter(tail, "ignored")?,
        teardown_failed: counter(tail, "teardown failed")?,
        measured: counter(tail, "measured")?,
        total: counter(tail, "total")?,
        filtered_out: counter(tail, "filtered out")?,
    })
}

/// Assert that `output`'s summary counters equal `expected` exactly.
///
/// # Errors
///
/// Fails when the summary cannot be parsed or any counter differs.
pub fn assert_summary(output: &str, expected: RunSummary) -> Result<()> {
    let actual =
        parse_summary(output).with_context(|| format!("parsing runner summary from:\n{output}"))?;
    anyhow::ensure!(
        actual == expected,
        "runner summary mismatch.\n  expected: {expected:?}\n  actual:   {actual:?}\n--- output ---\n{output}",
    );
    Ok(())
}

/// Assert that `output` reports exactly the given set of executed test
/// names — nothing missing, nothing extra, no foreign names.
///
/// `expected` is the full list of executed test names *with
/// multiplicity*: a test that runs once under each of two runtimes is
/// listed twice. The comparison is order-independent and
/// multiplicity-sensitive, so a lost test, a leaked test, and a
/// double-executed test all fail.
///
/// Names are compared by their qualified form (`module::path::test`),
/// extracted from every `[{TAG}]` result line. Suite `setup`/`teardown`
/// lifecycle lines are excluded.
///
/// # Errors
///
/// Fails when the executed-name multiset differs from `expected`.
pub fn assert_exact_test_names(output: &str, expected: &[&str]) -> Result<()> {
    let mut actual: BTreeMap<String, usize> = BTreeMap::new();
    for name in output.lines().filter_map(test_name_from_result_line) {
        *actual.entry(name).or_insert(0) += 1;
    }
    let mut wanted: BTreeMap<String, usize> = BTreeMap::new();
    for name in expected {
        *wanted.entry((*name).to_owned()).or_insert(0) += 1;
    }
    anyhow::ensure!(
        actual == wanted,
        "executed-test-name set mismatch.\n  expected: {wanted:?}\n  actual:   {actual:?}\n--- output ---\n{output}",
    );
    Ok(())
}

/// Locate the first `test result:` line and return the counter tail —
/// the text after the `ok.` / `FAILED.` label.
fn summary_tail(output: &str) -> Result<&str> {
    let line = output
        .lines()
        .find(|candidate| candidate.contains("test result:"))
        .ok_or_else(|| anyhow::anyhow!("no `test result:` summary line in output"))?;
    let after_label = line
        .trim()
        .strip_prefix("test result:")
        .ok_or_else(|| anyhow::anyhow!("summary line lacks `test result:` prefix: {line:?}"))?
        .trim();
    let (_verdict, tail) = after_label
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("summary line lacks `ok.`/`FAILED.` label: {line:?}"))?;
    Ok(tail.trim())
}

/// Extract one counter by its full key from the summary tail
/// (`"3 passed; 0 failed; …; finished in 1.2s"`). Each `key value`
/// pair is `split(';')`; the key is everything after the first space
/// in each segment, so multi-word keys (`timed out`, `teardown
/// failed`, `filtered out`) match exactly and single-word keys never
/// collide with them.
fn counter(tail: &str, key: &str) -> Result<usize> {
    for segment in tail.split(';') {
        let segment = segment.trim();
        let Some((number, rest_key)) = segment.split_once(' ') else {
            continue;
        };
        if rest_key.trim() == key {
            return number.trim().parse::<usize>().with_context(|| {
                format!("summary counter `{key}` has non-integer value `{number:?}`")
            });
        }
    }
    anyhow::bail!("summary tail is missing the `{key}` counter: {tail}")
}

/// Result-line status tags the runner paints. `SETUP` is the per-test
/// context-failure tag (distinct from the `setup <suite>` lifecycle
/// lines this filter excludes).
const STATUS_TAGS: &[&str] = &[
    "OK", "FAIL", "PANIC", "TIMEOUT", "CANCEL", "IGNORE", "BENCH", "SETUP", "HANG",
];

/// Extract the qualified test name from a `[{TAG}] {name} <…>` result
/// line, skipping suite `setup`/`teardown` lifecycle lines and any
/// non-result line. Returns `None` for lines that are not per-test
/// result lines.
fn test_name_from_result_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let after_bracket = trimmed.strip_prefix('[')?;
    let (tag, rest) = after_bracket.split_once(']')?;
    if !STATUS_TAGS.contains(&tag) {
        return None;
    }
    let display = rest.trim();
    // Suite lifecycle lines (`[OK] setup <suite>` / `[OK] teardown
    // <suite>`) carry a status tag but are not test bodies.
    if display.starts_with("setup ") || display.starts_with("teardown ") {
        return None;
    }
    // The trailing `<runtime, elapsed>` block always follows the name.
    let name = display.split('<').next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}
