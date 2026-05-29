//! Deterministic, syn-based, hybrid AST+text Rust source transformer
//! used to retype non-`Result`-returning fns to `anyhow::Result<T>`.
//!
//! # Approach
//!
//! Pure `quote!` regeneration drops comments and formatting. Instead
//! we parse with `syn::parse_file`, walk the AST collecting byte-range
//! `Edit`s against the original source, then apply them in descending
//! offset order so earlier edits do not shift later positions. The
//! result preserves every untouched byte verbatim.
//!
//! # Scope of changes per fn
//!
//! 1. Return type: `-> T` becomes `-> anyhow::Result<T>`; `()` (no
//!    `->`) gets `-> anyhow::Result<()>` inserted; `-> impl Trait`
//!    becomes `-> anyhow::Result<impl Trait>` and the wrap-site gets
//!    a `FIXME(rs-cleanup): verify Ok-wrap compiles` comment because
//!    syn cannot statically know whether the tail expression
//!    produces an `impl Trait` instance.
//!
//! 2. Body tail expression: wrapped in `Ok(...)` for value-returning
//!    fns; `()`-returning fns get `Ok(())` appended before the
//!    closing brace.
//!
//! 3. `return X;` statements: rewritten to `return Ok(X);`;
//!    `return;` becomes `return Ok(());`.
//!
//! 4. Hazard sites (per the `non-result-fns` taxonomy) get a
//!    `TODO(rs-cleanup)` comment inserted above the line, one per
//!    line per fn.
//!
//! 5. Intra-file call sites of fns this run converted get a
//!    `FIXME(rs-cleanup): callee returns Result, add ?` comment
//!    inserted above the call expression. The `?` insertion itself
//!    is left for human review — a Result-returning callee inside a
//!    non-Result context is exactly the kind of thing a human must
//!    decide how to propagate.
//!
//! # Edge cases (matching M-1 spec)
//!
//! - Trait impls: skipped wholesale when the trait's right-most path
//!   segment is in [`TRAIT_SKIP_LIST`]. Each skipped fn produces a
//!   `skipping <Trait>::<method>` log line.
//! - `const fn`: skipped — `anyhow::Result<T>` cannot be constructed
//!   in const context. Sets the run's exit code to 3.
//! - `fn name() -> !`: skipped — wrapping a never-returning fn in
//!   Result is meaningless.
//! - `async fn`: transformed identically to sync fns.
//! - `macro_rules!` and proc-macro item fns: skipped.
//! - Macros inside fn bodies: untouched. A single-macro body (e.g.
//!   `unimplemented!()`) for a `()`-returning fn gets `Ok(())`
//!   appended after the macro call.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use proc_macro2::Span;
use syn::spanned::Spanned as _;
use syn::visit::Visit;
use syn::{
    BinOp, Block, Expr, ExprCall, ExprPath, ExprReturn, ImplItem, Item, ItemFn, ItemImpl,
    ReturnType, Signature, Stmt, Type,
};

/// Traits whose impl-block fns are exempt from rewriting. Membership
/// criterion mirrors `linters/non-result-fns`: the trait signature is
/// locked by an external API (std, futures, etc.) so the impl cannot
/// return `Result<...>` even if the implementor would like to.
/// Matched by trait path right-most ident.
pub const TRAIT_SKIP_LIST: &[&str] = &[
    "Display",
    "Debug",
    "Binary",
    "Octal",
    "LowerHex",
    "UpperHex",
    "Pointer",
    "LowerExp",
    "UpperExp",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
    "Hasher",
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
    "Send",
    "Sync",
    "Unpin",
    "Sized",
    "Copy",
    "Clone",
    "Default",
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
    "Iterator",
    "IntoIterator",
    "FromIterator",
    "DoubleEndedIterator",
    "ExactSizeIterator",
    "FusedIterator",
    "Extend",
    "Sum",
    "Product",
    "Read",
    "Write",
    "Seek",
    "BufRead",
    "Future",
    "Stream",
    "AsyncRead",
    "AsyncWrite",
    "AsyncSeek",
    "AsyncBufRead",
];

/// One byte-range substitution recorded against the original source.
/// `start == end` denotes a pure insertion at offset `start`.
#[derive(Debug, Clone)]
struct Edit {
    /// Inclusive byte offset where the replacement begins.
    start: usize,
    /// Exclusive byte offset where the replacement ends. Equal to
    /// `start` for pure insertions.
    end: usize,
    /// Text spliced in at `start..end`.
    replacement: String,
    /// Monotonic insertion sequence number. Used as a tiebreaker
    /// when two edits share the same `start` so the apply order is
    /// deterministic regardless of upstream collection order.
    seq: usize,
}

/// Outcome of a single-file transformation. The caller decides how
/// to surface the new source (diff, write back, etc.).
#[derive(Debug)]
pub struct TransformOutcome {
    /// Full rewritten source. Identical to the input if nothing was
    /// transformed.
    pub new_source: String,
    /// Number of fns whose signature + body were rewritten.
    pub transformed: usize,
    /// Number of fns deliberately skipped (trait impls in skip list,
    /// const fns, never-return fns, already-Result fns, macros).
    pub skipped: usize,
    /// Number of `TODO(rs-cleanup)` comments inserted at hazard sites.
    pub hazard_todos: usize,
    /// Number of `FIXME(rs-cleanup)` comments inserted above
    /// intra-file call sites of converted fns.
    pub fixme_callsites: usize,
    /// Process exit code suggestion. `0` = clean, `3` = unsupported
    /// case encountered (e.g. const fn) that the human should review.
    pub exit_code: u8,
    /// Human-readable progress lines (skip / transform). Caller emits
    /// these to stderr.
    pub logs: Vec<String>,
}

/// Parse `source`, plan every edit, and return the rewritten text.
/// `source` must be a complete Rust file syn can parse; a parse
/// failure is bubbled up so the CLI can return exit code 2.
pub fn transform(source: &str) -> Result<TransformOutcome> {
    let parsed = syn::parse_file(source).map_err(|e| anyhow!("parse error: {e}"))?;

    let mut planner = Planner::new(source);
    planner.walk_items(&parsed.items);

    if !planner.converted_names.is_empty() {
        let mut visitor = CallSiteVisitor {
            converted: &planner.converted_names,
            sites: Vec::new(),
        };
        visitor.visit_file(&parsed);
        for span in visitor.sites {
            planner.push_fixme_callsite(span);
        }
    }

    let mut edits = planner.edits;
    edits.sort_by(|a, b| b.start.cmp(&a.start).then_with(|| b.seq.cmp(&a.seq)));

    let mut out = source.to_owned();
    for edit in edits {
        if edit.start > out.len() || edit.end > out.len() || edit.start > edit.end {
            return Err(anyhow!(
                "internal edit range out of bounds: {start}..{end} (len {len})",
                start = edit.start,
                end = edit.end,
                len = out.len(),
            ));
        }
        out.replace_range(edit.start..edit.end, &edit.replacement);
    }

    Ok(TransformOutcome {
        new_source: out,
        transformed: planner.transformed,
        skipped: planner.skipped,
        hazard_todos: planner.hazard_todos,
        fixme_callsites: planner.fixme_callsites,
        exit_code: planner.exit_code,
        logs: planner.logs,
    })
}

/// Per-file mutable state threaded through the walker.
struct Planner<'src> {
    source: &'src str,
    edits: Vec<Edit>,
    logs: Vec<String>,
    converted_names: BTreeSet<String>,
    transformed: usize,
    skipped: usize,
    hazard_todos: usize,
    fixme_callsites: usize,
    exit_code: u8,
    next_seq: usize,
}

impl<'src> Planner<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
            logs: Vec::new(),
            converted_names: BTreeSet::new(),
            transformed: 0,
            skipped: 0,
            hazard_todos: 0,
            fixme_callsites: 0,
            exit_code: 0,
            next_seq: 0,
        }
    }

    fn push_edit(&mut self, start: usize, end: usize, replacement: String) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.edits.push(Edit {
            start,
            end,
            replacement,
            seq,
        });
    }

    /// Walk a slice of items, descending into modules, trait blocks,
    /// and inherent / trait impls. Trait-impl skip logic lives in
    /// [`Self::walk_impl`]; this function only dispatches.
    fn walk_items(&mut self, items: &[Item]) {
        for item in items {
            match item {
                Item::Fn(f) => {
                    if has_proc_macro_attr(&f.attrs) {
                        self.skipped = self.skipped.saturating_add(1);
                        continue;
                    }
                    self.process_fn(&f.sig, &f.block);
                }
                Item::Impl(imp) => {
                    self.walk_impl(imp);
                }
                Item::Trait(tr) => {
                    for trait_item in &tr.items {
                        if let syn::TraitItem::Fn(f) = trait_item
                            && let Some(block) = f.default.as_ref()
                        {
                            // Trait default methods: rewrite as if
                            // they were free fns. Implementors can
                            // override, so the default carries the
                            // hazard surface.
                            self.process_fn(&f.sig, block);
                        }
                    }
                }
                Item::Mod(m) => {
                    if let Some((_, sub_items)) = &m.content {
                        self.walk_items(sub_items);
                    }
                }
                Item::Macro(_) => {
                    // `macro_rules!` and other item-level macro
                    // invocations are opaque to syn beyond their
                    // surface tokens. Leaving them untouched is the
                    // only safe option.
                    self.skipped = self.skipped.saturating_add(1);
                }
                _ => {}
            }
        }
    }

    fn walk_impl(&mut self, imp: &ItemImpl) {
        let (skipped_trait, trait_label) = match &imp.trait_ {
            Some((_, path, _)) => {
                let last = path.segments.last().map(|s| s.ident.to_string());
                let in_skip = last
                    .as_deref()
                    .is_some_and(|name| TRAIT_SKIP_LIST.contains(&name));
                (in_skip, last)
            }
            None => (false, None),
        };
        for impl_item in &imp.items {
            if let ImplItem::Fn(f) = impl_item {
                if has_proc_macro_attr(&f.attrs) {
                    self.skipped = self.skipped.saturating_add(1);
                    continue;
                }
                if skipped_trait {
                    self.log_skip_trait(trait_label.as_deref(), &f.sig);
                    self.skipped = self.skipped.saturating_add(1);
                    continue;
                }
                self.process_fn(&f.sig, &f.block);
            }
        }
    }

    fn log_skip_trait(&mut self, trait_label: Option<&str>, sig: &Signature) {
        let trait_name = trait_label.unwrap_or("<trait>");
        let method = sig.ident.to_string();
        let line = sig.fn_token.span.start().line;
        self.logs
            .push(format!("skipping {trait_name}::{method} at line {line}"));
    }

    fn process_fn(&mut self, sig: &Signature, block: &Block) {
        if sig.constness.is_some() {
            let name = sig.ident.to_string();
            let line = sig.fn_token.span.start().line;
            self.logs.push(format!(
                "skipping const fn {name} at line {line}: anyhow::Result<T> cannot be constructed in const context"
            ));
            self.skipped = self.skipped.saturating_add(1);
            self.exit_code = 3;
            return;
        }

        let return_mode = classify_return(&sig.output);
        let mode = match return_mode {
            ReturnClass::AlreadyResult | ReturnClass::Never => {
                self.skipped = self.skipped.saturating_add(1);
                if matches!(return_mode, ReturnClass::Never) {
                    let name = sig.ident.to_string();
                    let line = sig.fn_token.span.start().line;
                    self.logs
                        .push(format!("skipping never-return fn {name} at line {line}"));
                }
                return;
            }
            ReturnClass::Unit => TransformMode::Unit,
            ReturnClass::Value => TransformMode::Value,
            ReturnClass::ImplTrait => TransformMode::ImplTrait,
        };

        self.rewrite_signature(sig, mode);
        self.rewrite_body(block, mode);
        self.collect_hazards(block);

        self.converted_names.insert(sig.ident.to_string());
        self.transformed = self.transformed.saturating_add(1);
        self.logs.push(format!(
            "transformed fn {name} at line {line}",
            name = sig.ident,
            line = sig.fn_token.span.start().line,
        ));
    }

    fn rewrite_signature(&mut self, sig: &Signature, mode: TransformMode) {
        match &sig.output {
            ReturnType::Default => {
                let paren_close = sig.paren_token.span.close().byte_range();
                self.push_edit(paren_close.end, paren_close.end, " -> anyhow::Result<()>".to_owned());
            }
            ReturnType::Type(arrow, ty) => {
                let _ = mode;
                let start = arrow.span().byte_range().start;
                let end = ty.span().byte_range().end;
                let ty_text = self
                    .source
                    .get(ty.span().byte_range())
                    .unwrap_or("()");
                self.push_edit(start, end, format!("-> anyhow::Result<{ty_text}>"));
            }
        }
    }

    fn rewrite_body(&mut self, block: &Block, mode: TransformMode) {
        // Rewrite every `return X;` / `return;` reachable in this
        // fn's scope (excluding closures and nested item fns).
        let mut return_visitor = ReturnVisitor { returns: Vec::new() };
        return_visitor.visit_block(block);
        for ret in return_visitor.returns {
            self.rewrite_return(&ret);
        }

        // Classify the tail. `Stmt::Expr(_, None)` is a wrappable
        // expression tail. `Stmt::Macro` without a trailing semi is
        // the `unimplemented!()`-as-tail shape: it needs `;` plus
        // `Ok(())` appended in the Unit case, and is wrappable in
        // the value case (the macro's `!` type coerces into anything).
        let close_brace = block.brace_token.span.close().byte_range();
        let tail = block.stmts.last();
        let tail_kind = match tail {
            Some(Stmt::Expr(Expr::Return(_), None)) => TailKind::Return,
            Some(Stmt::Expr(expr, None)) => TailKind::Expr(expr.span()),
            Some(Stmt::Macro(m)) if m.semi_token.is_none() => TailKind::Expr(m.mac.span()),
            _ => TailKind::None,
        };

        match (mode, tail_kind) {
            (_, TailKind::Return) => {
                // Already handled by the return pass.
            }
            (TransformMode::Unit, TailKind::Expr(span)) => {
                let end = span.byte_range().end;
                self.push_edit(end, end, ";".to_owned());
                self.push_edit(
                    close_brace.start,
                    close_brace.start,
                    "    Ok(())\n".to_owned(),
                );
            }
            (TransformMode::Unit, TailKind::None) => {
                self.push_edit(
                    close_brace.start,
                    close_brace.start,
                    "    Ok(())\n".to_owned(),
                );
            }
            (TransformMode::Value | TransformMode::ImplTrait, TailKind::Expr(span)) => {
                let range = span.byte_range();
                if matches!(mode, TransformMode::ImplTrait) {
                    let (line_start, indent) = line_start_with_indent(self.source, range.start);
                    self.push_edit(
                        line_start,
                        line_start,
                        format!(
                            "{indent}// FIXME(rs-cleanup): tail expr returns impl Trait; verify Ok-wrap compiles\n"
                        ),
                    );
                }
                self.push_edit(range.start, range.start, "Ok(".to_owned());
                self.push_edit(range.end, range.end, ")".to_owned());
            }
            (TransformMode::Value | TransformMode::ImplTrait, TailKind::None) => {
                // A value-returning fn whose last statement is
                // semi-terminated or `let`/nested-item cannot
                // compile without an explicit `return` somewhere
                // upstream. Flag for human review.
                self.push_edit(
                    close_brace.start,
                    close_brace.start,
                    "    // FIXME(rs-cleanup): non-Unit fn has no tail expression; add `Ok(...)` manually\n".to_owned(),
                );
            }
        }
    }

    fn rewrite_return(&mut self, ret: &ExprReturn) {
        let return_kw = ret.return_token.span.byte_range();
        match &ret.expr {
            None => {
                self.push_edit(return_kw.end, return_kw.end, " Ok(())".to_owned());
            }
            Some(expr) => {
                let r = expr.span().byte_range();
                self.push_edit(r.start, r.start, "Ok(".to_owned());
                self.push_edit(r.end, r.end, ")".to_owned());
            }
        }
    }

    fn collect_hazards(&mut self, block: &Block) {
        let mut hz = HazardVisitor {
            sites: Vec::new(),
        };
        hz.visit_block(block);
        let mut seen_lines: BTreeSet<usize> = BTreeSet::new();
        for site in hz.sites {
            if !seen_lines.insert(site.line) {
                continue;
            }
            let (line_start, indent) = line_start_with_indent(self.source, site.byte_offset);
            let kind = site.kind.label();
            self.push_edit(
                line_start,
                line_start,
                format!(
                    "{indent}// TODO(rs-cleanup): {kind} at line {ln} — review and replace with Result-propagating equivalent\n",
                    ln = site.line,
                ),
            );
            self.hazard_todos = self.hazard_todos.saturating_add(1);
        }
    }

    fn push_fixme_callsite(&mut self, span: Span) {
        let r = span.byte_range();
        let (line_start, indent) = line_start_with_indent(self.source, r.start);
        self.push_edit(
            line_start,
            line_start,
            format!("{indent}// FIXME(rs-cleanup): callee returns Result, add ?\n"),
        );
        self.fixme_callsites = self.fixme_callsites.saturating_add(1);
    }
}

#[derive(Debug, Clone, Copy)]
enum TransformMode {
    Unit,
    Value,
    ImplTrait,
}

/// Tail-statement classification. `Span` carries the span of the
/// wrappable expression (or macro) so the rewrite_body match arms
/// can directly compute byte ranges without re-inspecting the stmt.
enum TailKind {
    /// Tail is a `return ...` — handled by the return-rewrite pass.
    Return,
    /// Tail is a wrappable expression (or macro without semi).
    Expr(Span),
    /// No tail expression — the block ends with a semi-terminated
    /// statement, a `let`, or a nested item.
    None,
}

enum ReturnClass {
    AlreadyResult,
    Never,
    Unit,
    Value,
    ImplTrait,
}

fn classify_return(rt: &ReturnType) -> ReturnClass {
    match rt {
        ReturnType::Default => ReturnClass::Unit,
        ReturnType::Type(_, ty) => match ty.as_ref() {
            Type::Never(_) => ReturnClass::Never,
            Type::ImplTrait(_) => ReturnClass::ImplTrait,
            Type::Path(p) => {
                let last = p.path.segments.last().map(|s| s.ident.to_string());
                if last.as_deref() == Some("Result") {
                    ReturnClass::AlreadyResult
                } else {
                    ReturnClass::Value
                }
            }
            Type::Tuple(t) if t.elems.is_empty() => ReturnClass::Unit,
            _ => ReturnClass::Value,
        },
    }
}

fn has_proc_macro_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        let last = a.path().segments.last().map(|s| s.ident.to_string());
        matches!(
            last.as_deref(),
            Some("proc_macro" | "proc_macro_attribute" | "proc_macro_derive")
        )
    })
}

/// Return the byte offset of the start of the line containing
/// `pos`, plus the leading whitespace of that line as a String for
/// indent-preserving comment insertion.
fn line_start_with_indent(source: &str, pos: usize) -> (usize, String) {
    let head = source.get(..pos).unwrap_or("");
    let line_start = head.rfind('\n').map(|i| i.saturating_add(1)).unwrap_or(0);
    let prefix = source.get(line_start..pos).unwrap_or("");
    let indent: String = prefix
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    (line_start, indent)
}

/// Visitor that collects `return` expressions inside a fn body
/// without descending into closures or nested item fns (those have
/// their own return scopes).
struct ReturnVisitor<'ast> {
    returns: Vec<&'ast ExprReturn>,
}

impl<'ast> Visit<'ast> for ReturnVisitor<'ast> {
    fn visit_expr_return(&mut self, node: &'ast ExprReturn) {
        self.returns.push(node);
        syn::visit::visit_expr_return(self, node);
    }
    fn visit_expr_closure(&mut self, _node: &'ast syn::ExprClosure) {
        // Returns inside a closure exit the closure, not the fn.
    }
    fn visit_item_fn(&mut self, _node: &'ast ItemFn) {
        // Nested fn items have independent return scope.
    }
}

#[derive(Debug, Clone, Copy)]
enum HazardKind {
    Unwrap,
    Expect,
    Silencing,
    Arithmetic,
    Indexing,
    PanicMacro,
    NumericCast,
    MaskingArith,
}

impl HazardKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Unwrap => "unwrap",
            Self::Expect => "expect",
            Self::Silencing => "silencing",
            Self::Arithmetic => "arithmetic",
            Self::Indexing => "indexing",
            Self::PanicMacro => "panic_macros",
            Self::NumericCast => "numeric_cast",
            Self::MaskingArith => "masking_arith",
        }
    }
}

struct HazardSite {
    kind: HazardKind,
    line: usize,
    byte_offset: usize,
}

/// Mirrors `linters/non-result-fns::HazardVisitor` but records site
/// positions so the rewriter can attach TODO comments at the right
/// line.
struct HazardVisitor {
    sites: Vec<HazardSite>,
}

impl HazardVisitor {
    fn record(&mut self, kind: HazardKind, span: Span) {
        let r = span.byte_range();
        self.sites.push(HazardSite {
            kind,
            line: span.start().line,
            byte_offset: r.start,
        });
    }
}

impl<'ast> Visit<'ast> for HazardVisitor {
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        let span = node.method.span();
        match name.as_str() {
            "unwrap" => self.record(HazardKind::Unwrap, span),
            "expect" => self.record(HazardKind::Expect, span),
            "unwrap_or" | "unwrap_or_default" | "unwrap_or_else" | "ok" | "err" | "ok_or"
            | "ok_or_else" => {
                self.record(HazardKind::Silencing, span);
            }
            other if other.starts_with("saturating_") || other.starts_with("wrapping_") => {
                self.record(HazardKind::MaskingArith, span);
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
            self.record(HazardKind::Arithmetic, node.span());
        }
        syn::visit::visit_expr_binary(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast syn::ExprIndex) {
        self.record(HazardKind::Indexing, node.span());
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
                self.record(HazardKind::PanicMacro, seg.ident.span());
            }
        }
        syn::visit::visit_macro(self, node);
    }

    fn visit_expr_cast(&mut self, node: &'ast syn::ExprCast) {
        if let Type::Path(p) = node.ty.as_ref()
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
                self.record(HazardKind::NumericCast, node.span());
            }
        }
        syn::visit::visit_expr_cast(self, node);
    }
}

/// Visitor that finds call expressions whose path's right-most
/// segment matches a fn this run converted. Only handles
/// `Expr::Call` (path call) — method-call dispatch is too dynamic
/// to resolve textually.
struct CallSiteVisitor<'a> {
    converted: &'a BTreeSet<String>,
    sites: Vec<Span>,
}

impl<'a, 'ast> Visit<'ast> for CallSiteVisitor<'a> {
    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Expr::Path(ExprPath { path, .. }) = node.func.as_ref()
            && let Some(seg) = path.segments.last()
            && self.converted.contains(&seg.ident.to_string())
        {
            self.sites.push(node.span());
        }
        syn::visit::visit_expr_call(self, node);
    }
}

/// Produce a readable per-line diff: ` ` for unchanged, `-` for
/// removed, `+` for added. Built on a standard LCS DP table — the
/// O(n*m) cost is acceptable for the few-hundred-line files this
/// tool runs on. Lines that are identical at the same index are
/// emitted as context with a leading space; differing runs emit
/// `-` and `+` lines side-by-side.
pub fn render_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0usize; n.saturating_add(1)]; m.saturating_add(1)];
    for i in 0..m {
        for j in 0..n {
            let cell = if a.get(i) == b.get(j) {
                dp.get(i)
                    .and_then(|row| row.get(j))
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1)
            } else {
                let up = dp
                    .get(i)
                    .and_then(|row| row.get(j.saturating_add(1)))
                    .copied()
                    .unwrap_or(0);
                let left = dp
                    .get(i.saturating_add(1))
                    .and_then(|row| row.get(j))
                    .copied()
                    .unwrap_or(0);
                up.max(left)
            };
            if let Some(row) = dp.get_mut(i.saturating_add(1))
                && let Some(slot) = row.get_mut(j.saturating_add(1))
            {
                *slot = cell;
            }
        }
    }

    let mut ops: Vec<(char, &str)> = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        let ai = a.get(i.saturating_sub(1)).copied().unwrap_or("");
        let bj = b.get(j.saturating_sub(1)).copied().unwrap_or("");
        if ai == bj {
            ops.push((' ', ai));
            i = i.saturating_sub(1);
            j = j.saturating_sub(1);
            continue;
        }
        let up = dp
            .get(i.saturating_sub(1))
            .and_then(|row| row.get(j))
            .copied()
            .unwrap_or(0);
        let left = dp
            .get(i)
            .and_then(|row| row.get(j.saturating_sub(1)))
            .copied()
            .unwrap_or(0);
        if up >= left {
            ops.push(('-', ai));
            i = i.saturating_sub(1);
        } else {
            ops.push(('+', bj));
            j = j.saturating_sub(1);
        }
    }
    while i > 0 {
        ops.push(('-', a.get(i.saturating_sub(1)).copied().unwrap_or("")));
        i = i.saturating_sub(1);
    }
    while j > 0 {
        ops.push(('+', b.get(j.saturating_sub(1)).copied().unwrap_or("")));
        j = j.saturating_sub(1);
    }
    ops.reverse();

    let mut out = String::from("--- before\n+++ after\n");
    for (op, line) in ops {
        out.push(op);
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    out
}

