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
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use quote::ToTokens;
use serde::Deserialize;
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

/// Crate names for each `SCAN_DIRS` entry, indexed in parallel. Used
/// to build the qualified prefix in `Finding.qualified_path` so
/// allowlist entries can target `<crate>::<mod>::<fn>` precisely.
/// Manual mapping rather than reading every `Cargo.toml`: linter
/// context is the rudzio workspace, the names are stable, and the
/// list lives next to `SCAN_DIRS` so future-PP edits both together.
const CRATE_NAMES: &[&str] = &[
    "rudzio",
    "cargo_rudzio",
    "cargo_rudzio_test_helpers",
    "rudzio_macro",
    "rudzio_macro_internals",
    "rudzio_migrate",
];

/// Output report path, relative to invocation cwd.
const REPORT_PATH: &str = "linters/non-result-fns-report.txt";

/// Allowlist file path, relative to invocation cwd. A missing file is
/// treated as an empty allowlist.
const ALLOWLIST_PATH: &str = "linters/non-result-fns/allowlist.toml";

/// Traits whose impl-block fns are exempt from the non-Result audit.
/// Membership criterion: the trait signature is locked by an external
/// API (std, futures, etc.) so the impl cannot return `Result<...>`
/// even if the implementor would like to. Matched by trait path
/// right-most ident, so `std::fmt::Display`, `core::fmt::Display`,
/// and bare `Display` all hit the same entry.
const TRAIT_SKIP_LIST: &[&str] = &[
    // std::fmt
    "Display",
    "Debug",
    "Binary",
    "Octal",
    "LowerHex",
    "UpperHex",
    "Pointer",
    "LowerExp",
    "UpperExp",
    // std::cmp
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    // std::hash
    "Hash",
    "Hasher",
    // std::ops (signature-locked)
    "Drop",
    "Deref",
    "DerefMut",
    "Index",
    "IndexMut",
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Rem",
    "Neg",
    "Not",
    "BitAnd",
    "BitOr",
    "BitXor",
    "Shl",
    "Shr",
    "AddAssign",
    "SubAssign",
    "MulAssign",
    "DivAssign",
    "RemAssign",
    "BitAndAssign",
    "BitOrAssign",
    "BitXorAssign",
    "ShlAssign",
    "ShrAssign",
    "Fn",
    "FnMut",
    "FnOnce",
    // std::marker / std::clone / std::default
    "Send",
    "Sync",
    "Unpin",
    "Sized",
    "Copy",
    "Clone",
    "Default",
    // std::convert / std::borrow
    "AsRef",
    "AsMut",
    "From",
    "Into",
    "TryFrom",
    "TryInto",
    "Borrow",
    "BorrowMut",
    "ToOwned",
    "ToString",
    // std::iter (also catches the defensive `fn next` case)
    "Iterator",
    "IntoIterator",
    "FromIterator",
    "DoubleEndedIterator",
    "ExactSizeIterator",
    "FusedIterator",
    "Extend",
    "Sum",
    "Product",
    // std::io (locked signatures)
    "Read",
    "Write",
    "Seek",
    "BufRead",
    // async traits
    "Future",
    "Stream",
    "AsyncRead",
    "AsyncWrite",
    "AsyncSeek",
    "AsyncBufRead",
];

/// One `[[allow]]` entry from `allowlist.toml`. Fully-qualified
/// `<crate>::<mod>::<fn>` path plus a written justification.
#[derive(Debug, Deserialize)]
struct AllowEntry {
    /// Fully-qualified fn path the entry covers.
    path: String,
    /// Human-authored reason (reviewed). Surfaced in the report so
    /// future-PP can grep allowlist rationale without leaving the
    /// linter output.
    reason: String,
}

/// Raw shape of `allowlist.toml`. Top-level `[[allow]]` array; entries
/// deserialise into [`AllowEntry`].
#[derive(Debug, Deserialize)]
struct AllowlistFile {
    /// Every `[[allow]]` entry; defaults to empty when the file is
    /// absent or contains no entries.
    #[serde(default)]
    allow: Vec<AllowEntry>,
}

/// Parsed allowlist indexed by qualified path. Reason kept as value
/// so the report can surface justification alongside the entry.
struct Allowlist {
    /// Fully-qualified fn path → human reason.
    by_path: BTreeMap<String, String>,
}

impl Allowlist {
    /// Empty allowlist — same effective state as a missing file.
    fn empty() -> Self {
        Self {
            by_path: BTreeMap::new(),
        }
    }

    /// True iff `qualified_path` is allowlisted.
    fn covers(&self, qualified_path: &str) -> bool {
        self.by_path.contains_key(qualified_path)
    }
}

/// Command-line flags parsed from `argv`. Both gates default to off
/// so the current `hazards > 0` behaviour stays the only failing
/// condition until a campaign phase activates the new gates.
struct CliFlags {
    /// `--gate-todos`: fail when any `TODO(rs-cleanup)` markers exist
    /// in the scanned tree.
    gate_todos: bool,
    /// `--gate-expects N`: fail when `#[expect(...)]` attr count
    /// exceeds `N`. `None` keeps the gate inactive.
    gate_expects: Option<usize>,
}

impl CliFlags {
    /// Both gates inactive.
    const fn empty() -> Self {
        Self {
            gate_todos: false,
            gate_expects: None,
        }
    }
}

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
#[derive(Debug, Clone, Copy)]
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
    /// All-zero counts — replaces the prior `Default::default()`
    /// per P-1 (no `Default` impls in workspace-internal types).
    const fn empty() -> Self {
        Self {
            unwrap: 0,
            expect: 0,
            silencing: 0,
            arithmetic: 0,
            indexing: 0,
            panic_macros: 0,
            numeric_cast: 0,
            masking_arith: 0,
        }
    }

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
    /// Fully-qualified `<crate>::<mod>::<Type?>::<fn>` path. Used
    /// to match against the allowlist and to give the report a
    /// stable address for each fn.
    qualified_path: String,
    return_type: String,
    category: ReturnCategory,
    hazards: HazardCounts,
    /// `true` iff `qualified_path` matched an allowlist entry. The
    /// finding is still recorded so the report can surface it under
    /// an "Allowlisted" header alongside its reason.
    allowlisted: bool,
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
        return HazardCounts::empty();
    };
    let mut visitor = HazardVisitor::new();
    visitor.visit_block(block);
    visitor.counts
}

/// Process one fn: skip Result-returning, otherwise record. The
/// `mod_path` slice provides the qualified prefix used to address the
/// fn from the allowlist (`crate::mod1::mod2::Type::fn_name`).
fn record(
    sig: &Signature,
    body: Option<&syn::Block>,
    path: &Path,
    mod_path: &[String],
    allowlist: &Allowlist,
    findings: &mut Vec<Finding>,
) {
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
    let qualified_path = if mod_path.is_empty() {
        name.clone()
    } else {
        format!("{prefix}::{name}", prefix = mod_path.join("::"))
    };
    let allowlisted = allowlist.covers(&qualified_path);
    findings.push(Finding {
        path: path.to_path_buf(),
        line,
        qualified_path,
        return_type,
        category,
        hazards,
        allowlisted,
    });
}

/// Right-most segment ident of a trait path in an `impl Trait for T`
/// header. Returns `None` for inherent impls.
fn trait_last_segment(imp: &syn::ItemImpl) -> Option<String> {
    let (_, path, _) = imp.trait_.as_ref()?;
    Some(path.segments.last()?.ident.to_string())
}

/// Render the `Self` type of an `impl` block as a single string. Used
/// to extend the module path with the concrete type, so fns inside
/// `impl Foo { ... }` are addressable as `crate::mod::Foo::fn_name`.
fn impl_self_type_label(imp: &syn::ItemImpl) -> Option<String> {
    if let Type::Path(p) = imp.self_ty.as_ref() {
        return p.path.segments.last().map(|seg| seg.ident.to_string());
    }
    None
}

/// Walk every item in `items` recursively, recording any fn with
/// non-Result return. `mod_path` accumulates the qualified prefix
/// (crate + module idents + impl-type ident); the visitor pushes and
/// pops as it descends so each fn carries the correct qualified path.
/// Trait impls whose trait sits in [`TRAIT_SKIP_LIST`] are skipped
/// wholesale: their signatures are locked by the external trait and
/// cannot be retyped to `Result<...>`.
fn walk_items(
    items: &[Item],
    path: &Path,
    mod_path: &mut Vec<String>,
    allowlist: &Allowlist,
    findings: &mut Vec<Finding>,
) {
    for item in items {
        match item {
            Item::Fn(f) => record(&f.sig, Some(&f.block), path, mod_path, allowlist, findings),
            Item::Impl(imp) => {
                if trait_last_segment(imp)
                    .is_some_and(|trait_name| TRAIT_SKIP_LIST.contains(&trait_name.as_str()))
                {
                    continue;
                }
                let type_seg = impl_self_type_label(imp);
                if let Some(seg) = &type_seg {
                    mod_path.push(seg.clone());
                }
                for impl_item in &imp.items {
                    if let ImplItem::Fn(f) = impl_item {
                        record(
                            &f.sig,
                            Some(&f.block),
                            path,
                            mod_path,
                            allowlist,
                            findings,
                        );
                    }
                }
                if type_seg.is_some() {
                    let _popped = mod_path.pop();
                }
            }
            Item::Trait(tr) => {
                let type_seg = tr.ident.to_string();
                mod_path.push(type_seg);
                for trait_item in &tr.items {
                    if let TraitItem::Fn(f) = trait_item
                        && let Some(block) = f.default.as_ref()
                    {
                        record(&f.sig, Some(block), path, mod_path, allowlist, findings);
                    }
                }
                let _popped = mod_path.pop();
            }
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    let mod_name = m.ident.to_string();
                    mod_path.push(mod_name);
                    walk_items(items, path, mod_path, allowlist, findings);
                    let _popped = mod_path.pop();
                }
            }
            _ => {}
        }
    }
}

/// Build the qualified-module prefix for a source file. Crate name is
/// the head segment; each parent directory under the crate root
/// becomes an inner module segment; the file stem becomes the final
/// segment unless it is `lib`, `main`, or `mod` (those are the
/// canonical module-root file names and contribute nothing).
fn module_prefix_for_file(file: &Path, root: &Path, crate_name: &str) -> Vec<String> {
    let mut segs = vec![crate_name.to_owned()];
    let Ok(rel) = file.strip_prefix(root) else {
        return segs;
    };
    let components: Vec<_> = rel.iter().collect();
    if components.is_empty() {
        return segs;
    }
    let last_idx = components.len().saturating_sub(1);
    for (idx, comp) in components.iter().enumerate() {
        let seg = comp.to_string_lossy().to_string();
        if idx == last_idx {
            let stem = Path::new(&seg)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            if !matches!(stem.as_str(), "lib" | "main" | "mod") {
                segs.push(stem);
            }
        } else {
            segs.push(seg);
        }
    }
    segs
}

/// Per-file static-text counters. Visiting the file via syn counts
/// `#[expect(...)]` attributes; substring scan of the raw source
/// counts `TODO(rs-cleanup)` markers.
#[derive(Debug, Clone, Copy)]
struct TextCounters {
    /// `#[expect(...)]` attribute occurrences anywhere in the syn AST.
    expects: usize,
    /// `TODO(rs-cleanup)` substring occurrences in the raw text.
    todos: usize,
}

impl TextCounters {
    /// Zeroed counters.
    const fn empty() -> Self {
        Self { expects: 0, todos: 0 }
    }

    /// In-place merge of another counter set.
    fn add(&mut self, other: Self) {
        self.expects = self.expects.saturating_add(other.expects);
        self.todos = self.todos.saturating_add(other.todos);
    }
}

/// Visitor that counts every `#[expect(...)]` attribute reachable in
/// the AST. Mirrors the discovery surface of the expect-allow-ban
/// linter for cross-checks; the count is reported alongside hazards
/// so future-PP can watch the `#[expect]` total monotonically
/// decrease across P-0/M-2 phases.
struct ExpectVisitor {
    expects: usize,
}

impl ExpectVisitor {
    const fn new() -> Self {
        Self { expects: 0 }
    }
}

impl<'ast> Visit<'ast> for ExpectVisitor {
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr
            .path()
            .segments
            .last()
            .is_some_and(|seg| seg.ident == "expect")
        {
            self.expects = self.expects.saturating_add(1);
        }
        syn::visit::visit_attribute(self, attr);
    }
}

/// Scan one root directory recursively for `*.rs` files. Each file
/// gets its qualified-module prefix from [`module_prefix_for_file`];
/// the AST walk records fns plus accumulates `#[expect]` counts; a
/// substring pass counts `TODO(rs-cleanup)` markers.
fn scan_dir(
    root: &Path,
    crate_name: &str,
    allowlist: &Allowlist,
    findings: &mut Vec<Finding>,
    counters: &mut TextCounters,
) {
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
        let mut mod_path = module_prefix_for_file(path, root, crate_name);
        walk_items(&parsed.items, path, &mut mod_path, allowlist, findings);
        let mut expect_visitor = ExpectVisitor::new();
        expect_visitor.visit_file(&parsed);
        let todos = content.matches("TODO(rs-cleanup)").count();
        counters.add(TextCounters {
            expects: expect_visitor.expects,
            todos,
        });
    }
}

/// Read + parse `allowlist.toml`. Missing file → empty allowlist.
fn load_allowlist(cwd: &Path) -> Result<Allowlist> {
    let path = cwd.join(ALLOWLIST_PATH);
    if !path.exists() {
        return Ok(Allowlist::empty());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("read allowlist: {}", path.display()))?;
    let file: AllowlistFile = toml::from_str(&text)
        .with_context(|| format!("parse allowlist: {}", path.display()))?;
    let mut by_path = BTreeMap::new();
    for entry in file.allow {
        by_path.insert(entry.path, entry.reason);
    }
    Ok(Allowlist { by_path })
}

/// Parse `argv` into [`CliFlags`]. Unknown flags log a warning but do
/// not abort — the linter still runs and reports counts. Both gates
/// default to off.
fn parse_cli(args: &[String]) -> CliFlags {
    let mut flags = CliFlags::empty();
    let mut idx = 0;
    while idx < args.len() {
        let Some(arg) = args.get(idx) else {
            break;
        };
        match arg.as_str() {
            "--gate-todos" => flags.gate_todos = true,
            "--gate-expects" => {
                let value_idx = idx.saturating_add(1);
                if let Some(value) = args.get(value_idx) {
                    match value.parse::<usize>() {
                        Ok(n) => flags.gate_expects = Some(n),
                        Err(e) => {
                            eprintln!(
                                "non-result-fns: --gate-expects requires an unsigned integer, got {value:?}: {e}"
                            );
                        }
                    }
                    idx = value_idx;
                } else {
                    eprintln!("non-result-fns: --gate-expects requires a value");
                }
            }
            other => {
                eprintln!("non-result-fns: unknown flag {other:?}");
            }
        }
        idx = idx.saturating_add(1);
    }
    flags
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
/// then by path/line for stable diffs. Allowlisted findings emit
/// their qualified path and reason so future-PP can audit the
/// allowlist from the report alone.
fn render_report(findings: &[Finding], allowlist: &Allowlist, repo_root: &Path) -> String {
    let mut out = String::new();
    out.push_str("# Non-Result function audit\n");
    out.push_str("#\n");
    out.push_str("# Generated by linters/non-result-fns.\n");
    out.push_str("# Columns: hazard total, hazard breakdown, file:line, qualified path, signature.\n");
    out.push_str("# Hazard breakdown: unwrap/expect/silencing/arith/index/panic/cast/masking-arith\n");
    out.push_str("# `[allow]` prefix marks fns covered by allowlist.toml.\n");
    out.push('\n');

    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort_by(|a, b| {
        a.allowlisted
            .cmp(&b.allowlisted)
            .then_with(|| b.hazards.total().cmp(&a.hazards.total()))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });

    for f in &sorted {
        let rel = f.path.strip_prefix(repo_root).unwrap_or(&f.path);
        let h = &f.hazards;
        let allow_tag = if f.allowlisted { "[allow] " } else { "" };
        out.push_str(&format!(
            "{allow_tag}[{total:>3}] u{u}/e{e}/s{s}/a{a}/i{i}/p{p}/c{c}/m{m}  {path}:{line}  {qual}() -> {ret}\n",
            allow_tag = allow_tag,
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
            qual = f.qualified_path,
            ret = f.return_type,
        ));
    }

    let allowlisted: Vec<&Finding> = findings.iter().filter(|f| f.allowlisted).collect();
    if !allowlisted.is_empty() {
        out.push('\n');
        out.push_str("# Allowlist reasons:\n");
        for f in &allowlisted {
            if let Some(reason) = allowlist.by_path.get(&f.qualified_path) {
                out.push_str(&format!("#   {qual}: {reason}\n", qual = f.qualified_path));
            }
        }
    }
    out
}

/// Print summary to stdout: total fns, hazard breakdown, by-crate,
/// by-return-type, top hotspots, top timebombs. Allowlisted findings
/// are excluded from every aggregate so the report reflects the
/// active hazard surface, not the residual annotated one. The TODO
/// and `#[expect]` counters land at the top alongside hazards.
fn print_summary(
    findings: &[Finding],
    counters: &TextCounters,
    flags: &CliFlags,
    repo_root: &Path,
) {
    let total_fns = findings.iter().filter(|f| !f.allowlisted).count();
    let allowlisted_count = findings.iter().filter(|f| f.allowlisted).count();
    let mut totals = HazardCounts::empty();
    let mut by_cat: BTreeMap<ReturnCategory, usize> = BTreeMap::new();
    let mut by_crate_fns: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_crate_hazards: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_file_hazards: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for f in findings {
        if f.allowlisted {
            continue;
        }
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
    let with_hazards = findings
        .iter()
        .filter(|f| !f.allowlisted && f.hazards.total() > 0_usize)
        .count();

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
    println!("Allowlisted fns (excluded):   {allowlisted_count}");
    println!("Total hazard ops:             {total_hazards}");
    println!();
    println!("Text-marker counters:");
    println!(
        "  TODO(rs-cleanup):    {n}  (gate active: {gate})",
        n = counters.todos,
        gate = if flags.gate_todos { "yes" } else { "no" },
    );
    println!(
        "  #[expect(...)]:      {n}  (gate threshold: {gate})",
        n = counters.expects,
        gate = match flags.gate_expects {
            Some(n) => format!("{n}"),
            None => "inactive".to_owned(),
        },
    );
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
    println!("By return type (fn counts, allowlisted excluded):");
    let mut cats: Vec<_> = by_cat.iter().collect();
    cats.sort_by(|a, b| b.1.cmp(a.1));
    for (cat, count) in cats {
        println!("  {label:<18} {count}", label = cat.label(), count = count);
    }
    println!();
    println!("By crate (fns / total hazard ops, allowlisted excluded):");
    for (krate, fn_count) in &by_crate_fns {
        let haz = by_crate_hazards.get(krate).copied().unwrap_or(0_usize);
        println!("  {krate:<32}  fns={fn_count:<4}  hazards={haz}");
    }
    println!();
    println!("Top 10 hot-spot files (by hazard total):");
    let mut hot: Vec<_> = by_file_hazards.into_iter().collect();
    hot.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    for (path, haz) in hot.into_iter().take(10_usize) {
        let rel = path.strip_prefix(repo_root).unwrap_or(&path);
        println!("  {haz:>4}  {}", rel.display());
    }
    println!();
    println!("Top 15 individual timebombs (highest hazard fns):");
    let mut top: Vec<&Finding> = findings.iter().filter(|f| !f.allowlisted).collect();
    top.sort_by_key(|finding| std::cmp::Reverse(finding.hazards.total()));
    for f in top.into_iter().take(15_usize) {
        let rel = f.path.strip_prefix(repo_root).unwrap_or(&f.path);
        println!(
            "  [{:>3}]  {}:{}  {}() -> {}",
            f.hazards.total(),
            rel.display(),
            f.line,
            f.qualified_path,
            f.return_type,
        );
    }
    if allowlisted_count > 0_usize {
        println!();
        println!("Allowlisted fns ({allowlisted_count} entries) — excluded from gates:");
        for f in findings.iter().filter(|f| f.allowlisted) {
            let rel = f.path.strip_prefix(repo_root).unwrap_or(&f.path);
            println!(
                "  {qual}  ({path}:{line})",
                qual = f.qualified_path,
                path = rel.display(),
                line = f.line,
            );
        }
    }
}

fn main() -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("current_dir failed: {e}");
            return ExitCode::from(1);
        }
    };
    let argv: Vec<String> = env::args().skip(1).collect();
    let flags = parse_cli(&argv);
    let allowlist = match load_allowlist(&cwd) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("non-result-fns: allowlist load failed: {e:#}");
            return ExitCode::from(1);
        }
    };

    let mut findings: Vec<Finding> = Vec::new();
    let mut counters = TextCounters::empty();
    for (idx, dir) in SCAN_DIRS.iter().enumerate() {
        let full = cwd.join(dir);
        let crate_name = CRATE_NAMES.get(idx).copied().unwrap_or("unknown_crate");
        scan_dir(&full, crate_name, &allowlist, &mut findings, &mut counters);
    }

    let report = render_report(&findings, &allowlist, &cwd);
    let report_path = cwd.join(REPORT_PATH);
    if let Err(e) = fs::write(&report_path, &report) {
        eprintln!("write failed: {}: {e}", report_path.display());
        return ExitCode::from(1);
    }
    print_summary(&findings, &counters, &flags, &cwd);
    println!();
    println!("Detailed report: {}", report_path.display());

    // Gating: hazards always fail; TODO and #[expect] gates fail only
    // when explicitly enabled via CLI flags so phase activation is
    // opt-in (PLAN.md P-4: "initially the new gates are inactive;
    // activated phase-by-phase").
    let total_hazards: usize = findings
        .iter()
        .filter(|f| !f.allowlisted)
        .map(|f| f.hazards.total())
        .sum();
    let mut failed = false;

    if total_hazards > 0_usize {
        eprintln!();
        eprintln!(
            "FAIL: {total_hazards} hazard operations detected in {n} non-Result fn(s).",
            n = findings
                .iter()
                .filter(|f| !f.allowlisted && f.hazards.total() > 0_usize)
                .count(),
        );
        eprintln!(
            "Each is an escape hatch around the workspace lints (saturating_*/\
             wrapping_* mask overflow; unwrap_or-family swallows failure; etc.).\n\
             Resolve each by EITHER refactoring the containing fn to return Result \
             AND propagating the underlying error, OR by adding a localised \
             `#[expect(...)]` with a written justification after human review, OR \
             by adding the fn to allowlist.toml with a written justification."
        );
        eprintln!("Detailed report: {}", report_path.display());
        failed = true;
    }

    if flags.gate_todos && counters.todos > 0_usize {
        eprintln!();
        eprintln!(
            "FAIL (--gate-todos): {todos} `TODO(rs-cleanup)` marker(s) remain in the scanned tree.",
            todos = counters.todos,
        );
        failed = true;
    }

    if let Some(threshold) = flags.gate_expects
        && counters.expects > threshold
    {
        eprintln!();
        eprintln!(
            "FAIL (--gate-expects {threshold}): {expects} `#[expect(...)]` attribute(s) \
             exceed the threshold.",
            expects = counters.expects,
        );
        failed = true;
    }

    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
