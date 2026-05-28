//! Statistical instrument: enumerate every fn in the rudzio tree
//! whose return type is NOT `Result<...>`, score each by the count
//! of operations in its body that can silently misbehave in Rust
//! (arithmetic, indexing, `unwrap`/`expect`, silencing combinators,
//! panic macros, numeric `as` casts), rank by hazard total.
//!
//! # Hypothesis under test
//!
//! A function that does not return `Result` cannot propagate a
//! failure to its caller. Rust has many operations whose default
//! semantics are panic-on-failure (`a + b` overflows, `slice[i]`
//! out of bounds, `Option::unwrap` on `None`, etc.). A non-Result
//! function whose body contains those operations is statistically
//! a timebomb: the failure either panics (loud but unstructured)
//! or is silenced by `unwrap_or`-family combinators (quiet, the
//! Bug #16 shape). This instrument surfaces the candidates; a
//! human reviews and decides.
//!
//! # What's intentionally excluded
//!
//! - Function name. Names are noise — the criterion is structural,
//!   not lexical.
//! - Body length, cyclomatic complexity. Out of scope.
//! - Whether the panic-prone operation is actually reachable. Too
//!   expensive; the instrument is a coarse statistical net.
//!
//! # Scope
//!
//! Walks every `*.rs` file under the configured source roots, parses
//! each with `syn::parse_file`, recurses through nested `mod`/`impl`/
//! `trait` items, and records every fn whose return type does NOT
//! match the `Result<...>` family (`Result`, `anyhow::Result`,
//! `io::Result`, `syn::Result`, any path ending in `::Result<...>`).
//!
//! Excluded: trait method declarations without bodies (dispatch
//! points, not implementations), tests/examples/benches.
//!
//! # Output
//!
//! - Detailed sorted list to `linters/non-result-fns-report.txt`.
//! - Summary counts to stdout: total fns, total hazard ops, breakdown
//!   by hazard kind, by return type, by crate, top hot-spot files,
//!   top fn-level timebombs.
//!
//! # Exit code
//!
//! Exits non-zero iff any hazard ops are detected anywhere in the
//! scanned tree. Wired into `just ci`. The instrument is a hard
//! gate: any non-Result function whose body contains a silencing
//! combinator, masking arithmetic, panic macro, or other hazard
//! is a CI failure that must be either (a) refactored to return
//! `Result`, or (b) annotated with an explicit `#[expect(...)]`
//! after a human review — same trade-off as the existing
//! `arithmetic_side_effects` / `indexing_slicing` lints.
//!
//! No baseline / regression-only mode: the bar is zero. Any drift
//! upward is a regression by definition; any drift downward is
//! progress and lands without ceremony.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use quote::ToTokens;
use syn::visit::Visit;
use syn::{BinOp, ImplItem, Item, ReturnType, Signature, TraitItem, Type};
use walkdir::WalkDir;

/// Source roots scanned by the linter. Paths are relative to the
/// directory where the linter is invoked (expected to be the repo
/// root).
const SCAN_DIRS: &[&str] = &[
    "src",
    "cargo-rudzio/src",
    "cargo-rudzio-test-helpers/src",
    "macro/src",
    "macro-internals/src",
    "migrate/src",
];

/// Output report path, relative to invocation cwd.
const REPORT_PATH: &str = "linters/non-result-fns-report.txt";

/// Coarse bucket for the return-type frequency table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReturnCategory {
    Bool,
    OptionT,
    VecT,
    Map,
    Strings,
    Primitive,
    Unit,
    PathBuf,
    ImplIterator,
    Custom,
    Other,
}

impl ReturnCategory {
    /// Render as a column header in breakdowns.
    fn label(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::OptionT => "Option<T>",
            Self::VecT => "Vec<T>",
            Self::Map => "Map/Set",
            Self::Strings => "String/&str",
            Self::Primitive => "Numeric",
            Self::Unit => "()",
            Self::PathBuf => "PathBuf/&Path",
            Self::ImplIterator => "impl Iterator",
            Self::Custom => "Custom",
            Self::Other => "Other",
        }
    }
}

/// Per-fn body hazard counts. Each field counts occurrences of
/// the named hazard inside the fn body (and inside any nested
/// closures / blocks the visitor descends into).
#[derive(Debug, Default, Clone, Copy)]
struct HazardCounts {
    /// `.unwrap()` on Option/Result. Panics on None/Err.
    unwrap: usize,
    /// `.expect(...)` on Option/Result. Panics on None/Err.
    expect: usize,
    /// `.unwrap_or(...)` / `.unwrap_or_default()` / `.unwrap_or_else(...)`
    /// / `.ok()`. Silences a failure into a default — the Bug #16
    /// silent-fallthrough shape.
    silencing: usize,
    /// Arithmetic that can overflow / divide-by-zero: `+`, `-`,
    /// `*`, `/`, `%`, `<<`, `>>` (binary and compound-assignment
    /// forms). Even with `arithmetic_side_effects` forbidden at
    /// the workspace level, `#[expect(...)]` annotations can
    /// re-allow it locally.
    arithmetic: usize,
    /// Indexing `expr[expr]`. Panics on OOB.
    indexing: usize,
    /// `panic!`, `unreachable!`, `todo!`, `unimplemented!`,
    /// `assert!`, `debug_assert!`. Loud, but structurally
    /// equivalent to a non-recoverable failure path.
    panic_macros: usize,
    /// `expr as <numeric>` casts that silently truncate or wrap.
    numeric_cast: usize,
    /// Method calls of the `saturating_*` / `wrapping_*` family
    /// (`saturating_add`, `saturating_sub`, `saturating_mul`,
    /// `wrapping_add`, etc.). These mask overflow silently —
    /// `saturating_*` clamps to the type's bounds and pretends
    /// the operation succeeded; `wrapping_*` wraps modulo the
    /// type's range. Both lose the "overflow happened" signal.
    /// In a codebase where `arithmetic_side_effects` is forbidden,
    /// these are the typical escape hatches — reach for them to
    /// silence the lint, end up with masked bugs. The Result-shape
    /// alternative is `checked_*`, which returns `Option<T>`
    /// (or wrapping in `Result` at the call site).
    masking_arith: usize,
}

impl HazardCounts {
    /// Total hazard count for ranking.
    const fn total(&self) -> usize {
        self.unwrap
            + self.expect
            + self.silencing
            + self.arithmetic
            + self.indexing
            + self.panic_macros
            + self.numeric_cast
            + self.masking_arith
    }
}

/// `syn::visit::Visit` impl that walks a fn body and counts
/// hazards into a `HazardCounts`. Descends into closures, nested
/// blocks, expressions — every site where Rust's default
/// panic-on-failure semantics apply.
struct HazardVisitor {
    counts: HazardCounts,
}

impl HazardVisitor {
    /// Fresh visitor with zero counts.
    const fn new() -> Self {
        Self {
            counts: HazardCounts {
                unwrap: 0,
                expect: 0,
                silencing: 0,
                arithmetic: 0,
                indexing: 0,
                panic_macros: 0,
                numeric_cast: 0,
                masking_arith: 0,
            },
        }
    }
}

impl<'ast> Visit<'ast> for HazardVisitor {
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        match name.as_str() {
            "unwrap" => self.counts.unwrap = self.counts.unwrap.saturating_add(1),
            "expect" => self.counts.expect = self.counts.expect.saturating_add(1),
            "unwrap_or" | "unwrap_or_default" | "unwrap_or_else" | "ok" | "err"
            | "ok_or" | "ok_or_else" => {
                self.counts.silencing = self.counts.silencing.saturating_add(1);
            }
            other if other.starts_with("saturating_") || other.starts_with("wrapping_") => {
                // `saturating_*` clamps to type bounds, `wrapping_*`
                // wraps modulo the type's range. Both mask overflow
                // — the Result-shape alternative is `checked_*`,
                // which surfaces overflow as `None` (and lets the
                // caller decide whether to wrap in `Result`).
                self.counts.masking_arith =
                    self.counts.masking_arith.saturating_add(1);
            }
            _ => {}
        }
        syn::visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if matches!(
            node.op,
            BinOp::Add(_)
                | BinOp::Sub(_)
                | BinOp::Mul(_)
                | BinOp::Div(_)
                | BinOp::Rem(_)
                | BinOp::Shl(_)
                | BinOp::Shr(_)
                | BinOp::AddAssign(_)
                | BinOp::SubAssign(_)
                | BinOp::MulAssign(_)
                | BinOp::DivAssign(_)
                | BinOp::RemAssign(_)
                | BinOp::ShlAssign(_)
                | BinOp::ShrAssign(_)
        ) {
            self.counts.arithmetic = self.counts.arithmetic.saturating_add(1);
        }
        syn::visit::visit_expr_binary(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast syn::ExprIndex) {
        self.counts.indexing = self.counts.indexing.saturating_add(1);
        syn::visit::visit_expr_index(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if let Some(seg) = node.path.segments.last() {
            let name = seg.ident.to_string();
            if matches!(
                name.as_str(),
                "panic"
                    | "unreachable"
                    | "todo"
                    | "unimplemented"
                    | "assert"
                    | "debug_assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "debug_assert_eq"
                    | "debug_assert_ne"
            ) {
                self.counts.panic_macros = self.counts.panic_macros.saturating_add(1);
            }
        }
        syn::visit::visit_macro(self, node);
    }

    fn visit_expr_cast(&mut self, node: &'ast syn::ExprCast) {
        if let Type::Path(p) = &*node.ty
            && let Some(seg) = p.path.segments.last()
        {
            let name = seg.ident.to_string();
            if matches!(
                name.as_str(),
                "u8" | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "usize"
                    | "i8"
                    | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "isize"
                    | "f32"
                    | "f64"
            ) {
                self.counts.numeric_cast = self.counts.numeric_cast.saturating_add(1);
            }
        }
        syn::visit::visit_expr_cast(self, node);
    }
}

/// One non-Result fn finding.
struct Finding {
    path: PathBuf,
    line: usize,
    name: String,
    return_type: String,
    category: ReturnCategory,
    hazards: HazardCounts,
}

/// Last path segment ident, if the type is a path type.
fn last_segment_ident(ty: &Type) -> Option<String> {
    if let Type::Path(p) = ty {
        return p.path.segments.last().map(|seg| seg.ident.to_string());
    }
    None
}

/// True iff the return type is a `Result<...>` family (any path
/// ending in `::Result` or bare `Result`). Conservative.
fn is_result_type(ty: &Type) -> bool {
    matches!(last_segment_ident(ty).as_deref(), Some("Result"))
}

/// Render a type as a single-line string.
fn render_type(ty: &Type) -> String {
    let raw = ty.to_token_stream().to_string();
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bucket the return type for the frequency table.
fn categorize(ty: &Type, rendered: &str) -> ReturnCategory {
    if rendered == "bool" {
        return ReturnCategory::Bool;
    }
    if rendered.starts_with("Option <") || rendered.starts_with("Option<") {
        return ReturnCategory::OptionT;
    }
    if rendered.starts_with("Vec <") || rendered.starts_with("Vec<") {
        return ReturnCategory::VecT;
    }
    if rendered.starts_with("HashMap")
        || rendered.starts_with("BTreeMap")
        || rendered.starts_with("HashSet")
        || rendered.starts_with("BTreeSet")
    {
        return ReturnCategory::Map;
    }
    if rendered == "String"
        || rendered.contains("& str")
        || rendered.contains("&str")
        || rendered.contains("&'static str")
    {
        return ReturnCategory::Strings;
    }
    if matches!(
        rendered,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
            | "char"
    ) {
        return ReturnCategory::Primitive;
    }
    if rendered == "PathBuf" || rendered.contains("& Path") || rendered.contains("&Path") {
        return ReturnCategory::PathBuf;
    }
    if rendered.starts_with("impl Iterator") {
        return ReturnCategory::ImplIterator;
    }
    if matches!(ty, Type::Path(_)) {
        return ReturnCategory::Custom;
    }
    ReturnCategory::Other
}

/// Compute hazards for a fn body. `block` is `None` for trait
/// method decls without bodies (those are already filtered out
/// by the walker).
fn count_hazards(block: Option<&syn::Block>) -> HazardCounts {
    let Some(block) = block else {
        return HazardCounts::default();
    };
    let mut visitor = HazardVisitor::new();
    visitor.visit_block(block);
    visitor.counts
}

/// Process one fn: skip Result-returning, otherwise record.
fn record(sig: &Signature, body: Option<&syn::Block>, path: &Path, findings: &mut Vec<Finding>) {
    let name = sig.ident.to_string();
    let line = sig.fn_token.span.start().line;
    let (return_type, category) = match &sig.output {
        ReturnType::Default => ("()".to_owned(), ReturnCategory::Unit),
        ReturnType::Type(_, ty) => {
            if is_result_type(ty) {
                return;
            }
            let rendered = render_type(ty);
            let category = categorize(ty, &rendered);
            (rendered, category)
        }
    };
    let hazards = count_hazards(body);
    findings.push(Finding {
        path: path.to_path_buf(),
        line,
        name,
        return_type,
        category,
        hazards,
    });
}

/// Walk every item in `items` recursively, recording any fn with
/// non-Result return.
fn walk_items(items: &[Item], path: &Path, findings: &mut Vec<Finding>) {
    for item in items {
        match item {
            Item::Fn(f) => record(&f.sig, Some(&f.block), path, findings),
            Item::Impl(imp) => {
                for impl_item in &imp.items {
                    if let ImplItem::Fn(f) = impl_item {
                        record(&f.sig, Some(&f.block), path, findings);
                    }
                }
            }
            Item::Trait(tr) => {
                for trait_item in &tr.items {
                    if let TraitItem::Fn(f) = trait_item
                        && let Some(block) = f.default.as_ref()
                    {
                        record(&f.sig, Some(block), path, findings);
                    }
                }
            }
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    walk_items(items, path, findings);
                }
            }
            _ => {}
        }
    }
}

/// Scan one root directory recursively for `*.rs` files.
fn scan_dir(root: &Path, findings: &mut Vec<Finding>) {
    if !root.is_dir() {
        eprintln!("skip: {} (not a directory)", root.display());
        return;
    }
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("read failed: {}: {e}", path.display());
                continue;
            }
        };
        let parsed = match syn::parse_file(&content) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("parse failed: {}: {e}", path.display());
                continue;
            }
        };
        walk_items(&parsed.items, path, findings);
    }
}

/// Top-level crate name from a path like `cargo-rudzio/src/foo.rs`.
fn crate_label(path: &Path, repo_root: &Path) -> String {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    rel.components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_else(|| "?".to_owned())
}

/// Render the detailed report. Sorted by hazard total descending,
/// then by path/line for stable diffs.
fn render_report(findings: &[Finding], repo_root: &Path) -> String {
    let mut out = String::new();
    out.push_str("# Non-Result function audit\n");
    out.push_str("#\n");
    out.push_str("# Generated by linters/non-result-fns.\n");
    out.push_str("# Columns: hazard total, hazard breakdown, file:line, signature.\n");
    out.push_str("# Hazard breakdown: unwrap/expect/silencing/arith/index/panic/cast/masking-arith\n");
    out.push_str("# Functions with hazard=0 are listed at the bottom (likely safe).\n");
    out.push('\n');

    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort_by(|a, b| {
        b.hazards
            .total()
            .cmp(&a.hazards.total())
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });

    for f in &sorted {
        let rel = f.path.strip_prefix(repo_root).unwrap_or(&f.path);
        let h = &f.hazards;
        out.push_str(&format!(
            "[{total:>3}] u{u}/e{e}/s{s}/a{a}/i{i}/p{p}/c{c}/m{m}  {path}:{line}  fn {name}() -> {ret}\n",
            total = h.total(),
            u = h.unwrap,
            e = h.expect,
            s = h.silencing,
            a = h.arithmetic,
            i = h.indexing,
            p = h.panic_macros,
            c = h.numeric_cast,
            m = h.masking_arith,
            path = rel.display(),
            line = f.line,
            name = f.name,
            ret = f.return_type,
        ));
    }
    out
}

/// Print summary to stdout: total fns, hazard breakdown, by-crate,
/// by-return-type, top hotspots, top timebombs.
fn print_summary(findings: &[Finding], repo_root: &Path) {
    let total_fns = findings.len();
    let mut totals = HazardCounts::default();
    let mut by_cat: BTreeMap<ReturnCategory, usize> = BTreeMap::new();
    let mut by_crate_fns: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_crate_hazards: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_file_hazards: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for f in findings {
        totals.unwrap = totals.unwrap.saturating_add(f.hazards.unwrap);
        totals.expect = totals.expect.saturating_add(f.hazards.expect);
        totals.silencing = totals.silencing.saturating_add(f.hazards.silencing);
        totals.arithmetic = totals.arithmetic.saturating_add(f.hazards.arithmetic);
        totals.indexing = totals.indexing.saturating_add(f.hazards.indexing);
        totals.panic_macros = totals.panic_macros.saturating_add(f.hazards.panic_macros);
        totals.numeric_cast = totals.numeric_cast.saturating_add(f.hazards.numeric_cast);
        totals.masking_arith = totals.masking_arith.saturating_add(f.hazards.masking_arith);
        *by_cat.entry(f.category).or_insert(0_usize) =
            by_cat.get(&f.category).copied().unwrap_or(0_usize).saturating_add(1_usize);
        let krate = crate_label(&f.path, repo_root);
        *by_crate_fns.entry(krate.clone()).or_insert(0_usize) = by_crate_fns
            .get(&krate)
            .copied()
            .unwrap_or(0_usize)
            .saturating_add(1_usize);
        *by_crate_hazards.entry(krate.clone()).or_insert(0_usize) = by_crate_hazards
            .get(&krate)
            .copied()
            .unwrap_or(0_usize)
            .saturating_add(f.hazards.total());
        let key = f.path.clone();
        *by_file_hazards.entry(key.clone()).or_insert(0_usize) = by_file_hazards
            .get(&key)
            .copied()
            .unwrap_or(0_usize)
            .saturating_add(f.hazards.total());
    }
    let total_hazards = totals.total();
    let with_hazards = findings.iter().filter(|f| f.hazards.total() > 0_usize).count();

    println!("Non-Result function audit — summary");
    println!("===================================");
    println!("Total non-Result fns:         {total_fns}");
    println!(
        "  with hazard ops in body:    {with_hazards} ({pct}%)",
        pct = if total_fns == 0_usize {
            0_usize
        } else {
            (with_hazards.saturating_mul(100_usize)).saturating_div(total_fns)
        }
    );
    println!("  with no hazard ops in body: {clean}", clean = total_fns.saturating_sub(with_hazards));
    println!("Total hazard ops:             {total_hazards}");
    println!();
    println!("Hazard ops by kind:");
    println!("  unwrap()           {n}", n = totals.unwrap);
    println!("  expect(...)        {n}", n = totals.expect);
    println!("  silencing combo    {n}  (unwrap_or, unwrap_or_default, unwrap_or_else, ok, err, ok_or, ok_or_else)", n = totals.silencing);
    println!("  arithmetic op      {n}  (+, -, *, /, %, <<, >>, and compound-assign variants)", n = totals.arithmetic);
    println!("  indexing expr      {n}  (expr[expr])", n = totals.indexing);
    println!("  panic-class macro  {n}  (panic!, unreachable!, todo!, unimplemented!, assert!, debug_assert!)", n = totals.panic_macros);
    println!("  numeric `as` cast  {n}", n = totals.numeric_cast);
    println!("  masking arithmetic {n}  (saturating_*, wrapping_* — mask overflow vs `checked_*` which surfaces it)", n = totals.masking_arith);
    println!();
    println!("By return type (fn counts):");
    let mut cats: Vec<_> = by_cat.iter().collect();
    cats.sort_by(|a, b| b.1.cmp(a.1));
    for (cat, count) in cats {
        println!("  {label:<18} {count}", label = cat.label(), count = count);
    }
    println!();
    println!("By crate (fns / total hazard ops):");
    for (krate, fn_count) in &by_crate_fns {
        let haz = by_crate_hazards.get(krate).copied().unwrap_or(0_usize);
        println!("  {krate:<32}  fns={fn_count:<4}  hazards={haz}");
    }
    println!();
    println!("Top 10 hot-spot files (by hazard total):");
    let mut hot: Vec<_> = by_file_hazards.into_iter().collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1));
    for (path, haz) in hot.into_iter().take(10_usize) {
        let rel = path.strip_prefix(repo_root).unwrap_or(&path);
        println!("  {haz:>4}  {}", rel.display());
    }
    println!();
    println!("Top 15 individual timebombs (highest hazard fns):");
    let mut top: Vec<&Finding> = findings.iter().collect();
    top.sort_by(|a, b| b.hazards.total().cmp(&a.hazards.total()));
    for f in top.into_iter().take(15_usize) {
        let rel = f.path.strip_prefix(repo_root).unwrap_or(&f.path);
        println!(
            "  [{:>3}]  {}:{}  fn {}() -> {}",
            f.hazards.total(),
            rel.display(),
            f.line,
            f.name,
            f.return_type,
        );
    }
}

fn main() {
    let cwd = match env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("current_dir failed: {e}");
            std::process::exit(1);
        }
    };
    let mut findings: Vec<Finding> = Vec::new();
    for dir in SCAN_DIRS {
        let full = cwd.join(dir);
        scan_dir(&full, &mut findings);
    }
    let report = render_report(&findings, &cwd);
    let report_path = cwd.join(REPORT_PATH);
    if let Err(e) = fs::write(&report_path, &report) {
        eprintln!("write failed: {}: {e}", report_path.display());
        std::process::exit(1);
    }
    print_summary(&findings, &cwd);
    println!();
    println!("Detailed report: {}", report_path.display());

    // Hard gate: any hazard op anywhere in a non-Result fn fails CI.
    // The instrument's exit code is the contract — CI runners surface
    // non-zero as failure with no further configuration.
    let total_hazards: usize = findings.iter().map(|f| f.hazards.total()).sum();
    if total_hazards > 0_usize {
        eprintln!();
        eprintln!(
            "FAIL: {total_hazards} hazard operations detected in {n} non-Result fn(s).",
            n = findings.iter().filter(|f| f.hazards.total() > 0_usize).count(),
        );
        eprintln!(
            "Each is an escape hatch around the workspace lints (saturating_*/\
             wrapping_* mask overflow; unwrap_or-family swallows failure; etc.).\n\
             Resolve each by EITHER refactoring the containing fn to return Result \
             AND propagating the underlying error, OR by adding a localised \
             `#[expect(...)]` with a written justification after human review."
        );
        eprintln!("Detailed report: {}", report_path.display());
        std::process::exit(1_i32);
    }
}
