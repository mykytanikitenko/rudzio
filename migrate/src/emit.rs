//! Read → parse → mutate → emit pipeline for a single Rust source
//! file.

use std::fs;
use std::panic;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use syn::spanned::Spanned as _;

use crate::backup;
use crate::cli::RuntimeChoice;
use crate::report::Report;
use crate::rewrite::{self, Outcome};
use crate::test_context::{self, Resolver as TestContextResolver};

/// Prefix of the magic doc-comment sentinel the rewriter emits for each
/// preserved-original block. The full sentinel is
/// `///PREFIX<index>SUFFIX`.
const SENTINEL_INNER_PREFIX: &str = "__RUDZIO_MIGRATE_ORIGINAL_PLACEHOLDER_";

/// Suffix of the magic doc-comment sentinel that identifies a
/// pre-migration placeholder line.
const SENTINEL_INNER_SUFFIX: &str = "__";

/// Bundle of inputs the [`process_file`] pipeline needs alongside the
/// path it's processing.
#[derive(Debug)]
#[non_exhaustive]
pub struct Options<'res> {
    /// Default runtime baked into generated suite blocks.
    pub default_runtime: RuntimeChoice,
    /// When `true`, parse and report but don't write anything to disk.
    pub dry_run: bool,
    /// When `true`, emit a pre-migration block comment above each
    /// converted fn.
    pub preserve_originals: bool,
    /// Resolver for `#[test_context(T)]` migrations.
    pub test_contexts: &'res TestContextResolver,
}

/// An `Item::Impl` that prettyplease can't render (because it carries an
/// `ImplItem::Verbatim`) replaced in-tree by a single-line placeholder
/// const.
///
/// `original_source` holds the exact bytes from the input file; after
/// prettyplease succeeds on the rest of the tree, the placeholder line
/// gets spliced back out and the original source gets stitched in — so
/// the impl survives the round-trip with formatting intact.
#[non_exhaustive]
struct SalvagedImpl {
    /// Index embedded in the placeholder's ident, used to find the
    /// placeholder line in prettyplease's output.
    index: usize,
    /// Exact byte-range text of the original impl from the input
    /// source, including its outer attributes.
    original_source: String,
}

/// Top-level entry: read `path`, run the rewriter, splice in any
/// preserved originals + ctx-bridge code, and write the result back to
/// disk (unless `opts.dry_run`).
///
/// # Errors
///
/// Returns the underlying I/O error if the file can't be read, written,
/// or backed up.
#[inline]
pub fn process_file(
    path: &Path,
    opts: &Options<'_>,
    report: &mut Report,
) -> Result<Option<Outcome>> {
    let source: Arc<str> =
        Arc::from(fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?);
    let mut tree: syn::File = match syn::parse_file(&source) {
        Ok(parsed) => parsed,
        Err(err) => {
            report.warn(
                path.to_path_buf(),
                Some(err.span().start().line),
                format!("syn parse failed: {err}; skipping file"),
            );
            return Ok(None);
        }
    };

    let rewrite = rewrite::apply(
        Arc::clone(&source),
        &mut tree,
        opts.default_runtime,
        opts.preserve_originals,
        opts.test_contexts,
        path,
        report,
    );

    // Even if the file itself had no test-fn rewrites, it may still be
    // the impl_file for one or more test-context plans and need bridge
    // impls appended.
    // FIXME(rs-cleanup): callee returns Result, add ?
    let bridge_suffix = render_bridge_for_file(path, opts.test_contexts);

    if !rewrite.changed && bridge_suffix.is_empty() {
        return Ok(None);
    }

    let mut output = if rewrite.changed {
        // Pre-pass: prettyplease panics on both `ImplItem::Verbatim`
        // AND `Item::Verbatim` (bodyless `fn X(&self);` items from
        // delegation macros like `#[ambassador::delegate_to_remote_methods]`).
        // Replace each offending `Item::Impl` with a single-line
        // placeholder constant whose name encodes the index, capturing
        // the impl's original source text separately. After
        // `prettyplease::unparse` succeeds, the placeholder line gets
        // spliced out and the original impl text is stitched in. The
        // rest of the file (the tests the user actually cares about)
        // unparses normally.
        let salvaged = salvage_verbatim_impls(&mut tree, &source)?;
        // Last-resort safety net: if prettyplease still panics on some
        // shape we didn't normalise, skip the whole rewrite with a
        // warning rather than aborting the run.
        let Ok(mut rendered) =
            panic::catch_unwind(panic::AssertUnwindSafe(|| prettyplease::unparse(&tree)))
        else {
            report.warn(
                path.to_path_buf(),
                None,
                "prettyplease::unparse panicked on this file (likely an ImplItem::Verbatim \u{2014} bodyless `fn X(&self);` from a macro such as `ambassador::delegate_to_remote_methods`); skipping the rewrite, original file left untouched",
            );
            return Ok(None);
        };
        if !salvaged.is_empty() {
            rendered = splice_salvaged_verbatim_impls(&rendered, &salvaged)?;
        }
        rendered
    } else {
        source.to_string()
    };
    if opts.preserve_originals && rewrite.changed {
        output = splice_preserved_originals(&output, &rewrite.original_snippets)?;
    }
    if !bridge_suffix.is_empty() {
        output = splice_bridge_before_first_suite_or_main(&output, &bridge_suffix)?;
    }

    if !opts.dry_run {
        let bak = backup::copy_before_write(path)
            .with_context(|| format!("backing up {}", path.display()))?;
        if matches!(bak, backup::Outcome::Created(_)) {
            report.backed_up(bak.path().to_path_buf());
        }
        fs::write(path, &output).with_context(|| format!("writing {}", path.display()))?;
        report.touched(path.to_path_buf());
    }

    Ok(Some(rewrite))
}

/// Walk backward from `const_pos` skipping blank lines and `#[...]`
/// attribute lines until we hit something else; return the
/// earliest-attribute byte offset (start of the line the first
/// encountered attribute lives on).
fn backward_scan_to_attrs(text: &str, const_pos: usize) -> Result<usize> {
    // `const_pos` points at `c` of the placeholder const; the newline
    // immediately before it (at const_pos-1) terminates the attribute
    // line we want to grab. Start earliest at the beginning of the
    // const line itself, then walk further back over each preceding
    // `#[...]` line.
    let mut earliest = const_pos;
    loop {
        // Exclude the newline right before `earliest`, otherwise
        // `rfind('\n')` returns the same newline forever.
        // saturating_sub floors at 0 (the start-of-file sentinel); this
        // is intentional, not a masked failure.
        let search_end = earliest.saturating_sub(1);
        // Real error path: `search_end` is derived from byte offsets into
        // `text`; an out-of-range slice means the offsets were corrupted.
        let prefix = text
            .get(..search_end)
            .context("backward_scan_to_attrs: search_end past end of text")?;
        let Some(prev_newline) = prefix.rfind('\n') else {
            // Reached start of file — nothing left to consume.
            if search_end == 0 {
                break;
            }
            let line = prefix.trim_start();
            if line.starts_with("#[") {
                earliest = 0;
            }
            break;
        };
        // +1 to step past the newline; bounded by search_end below.
        let line_start = prev_newline.saturating_add(1);
        // TODO(rs-cleanup): silencing — get/unwrap_or fall back to "" if
        // line_start > search_end (cannot happen: prev_newline < search_end).
        let line = text.get(line_start..search_end).unwrap_or("");
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[") {
            earliest = line_start;
        } else {
            break;
        }
    }
    Ok(earliest)
}

/// Compute the byte range of an item from the source by walking its
/// span. Uses `proc_macro2`'s `span-locations` feature; the range
/// covers the outermost attribute through the closing brace.
fn capture_item_source(item: &syn::Item, source: &str) -> Result<Option<String>> {
    let start = if let syn::Item::Impl(impl_block) = item {
        impl_block
            .attrs
            .iter()
            .map(|attr| attr.span().byte_range().start)
            .min()
            // TODO(rs-cleanup): silencing at line 209 — review and replace with Result-propagating equivalent
            .unwrap_or_else(|| impl_block.impl_token.span.byte_range().start)
    } else {
        item.span().byte_range().start
    };
    let end = item.span().byte_range().end;
    if start >= end {
        // Degenerate / empty span — nothing to capture (normal outcome).
        return Ok(None);
    }
    // Real error path: a span whose end exceeds the source length means
    // the byte offsets are corrupted, not merely "absent".
    let captured = source
        .get(start..end)
        .context("capture_item_source: item span exceeds source length")?;
    Ok(Some(captured.to_owned()))
}

/// Leading-spaces character count for `text`.
fn indent_at(text: &str, pos: usize) -> Result<String> {
    // Real error path: `pos` is a byte offset into `text`; out-of-range
    // means a corrupted offset.
    let prefix = text
        .get(..pos)
        .context("indent_at: pos past end of text")?;
    // +1 steps past the newline; map_or(0, ...) handles "no newline".
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx.saturating_add(1));
    // TODO(rs-cleanup): silencing — get/unwrap_or "" if line_start>pos
    // (cannot happen: line_start derived from a newline before pos).
    let line = text.get(line_start..pos).unwrap_or("");
    Ok(line.chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect())
}

/// Count of leading ASCII space bytes in `text`.
// NOTE(rs-cleanup): kept `-> usize` (not Result) — used inside
// iterator-chain closures (`.map(|line| leading_space_count(line))`),
// where a Result would force `.collect::<Result<_>>()?` gymnastics for
// no real failure mode. Linter-visible per the M-2 visibility policy.
fn leading_space_count(text: &str) -> Result<usize> {
    Ok(text.bytes().take_while(|byte| *byte == b' ').count())
}

/// Strip the common leading whitespace shared across non-empty lines so
/// the original's relative indentation is preserved while we place it
/// under the target indent.
fn normalize_snippet_indent(snippet: &str) -> Result<String> {
    let lines: Vec<&str> = snippet.lines().collect();
    if lines.len() <= 1 {
        return Ok(snippet.to_owned());
    }
    // The first line starts at the attribute byte, so it has no leading
    // indent; ignore it when computing common indent.
    let min_indent: usize = lines
        .iter()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        // FIXME(rs-cleanup): callee returns Result, add ?
        .map(|line| leading_space_count(line))
        .min()
        // TODO(rs-cleanup): silencing at line 252 — review and replace with Result-propagating equivalent
        .unwrap_or(0);
    let mut out = String::with_capacity(snippet.len());
    for (idx, line) in lines.iter().enumerate() {
        if idx == 0 {
            out.push_str(line);
        } else if line.trim().is_empty() {
            // Blank line: leave it as a bare newline so we preserve
            // separation without trying to dedent into nothing.
        } else {
            // FIXME(rs-cleanup): callee returns Result, add ?
            let skip = leading_space_count(line).min(min_indent);
            // Real error path: `skip` ≤ count of leading ASCII spaces, so
            // it always lands on a char boundary; an out-of-range slice
            // would mean the index math is broken.
            let dedented = line
                .get(skip..)
                .context("normalize_snippet_indent: dedent offset out of range")?;
            out.push_str(dedented);
        }
        out.push('\n');
    }
    Ok(out)
}

/// Match a pretty-printed line against the placeholder sentinel form
/// emitted by the rewriter; return `(indent, idx)` if it matches.
// NOTE(rs-cleanup): kept `-> Option<...>` (not Result) — a pure
// pattern-matcher where `None` means "this line is not a sentinel", a
// normal control-flow outcome, not an error. The internal `?`-on-Option
// expresses exactly that. Linter-visible per the M-2 visibility policy.
fn parse_sentinel_line(line: &str) -> Result<Option<(&str, usize)>> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    // TODO(rs-cleanup): masking_arith — saturating_sub of two trimmed
    // lengths cannot underflow; intentional.
    // TODO(rs-cleanup): masking_arith at line 312 — review and replace with Result-propagating equivalent
    let leading_len = trimmed.len().saturating_sub(trimmed.trim_start().len());
    // TODO(rs-cleanup): silencing — get/unwrap_or fall back to "".
    // TODO(rs-cleanup): silencing at line 314 — review and replace with Result-propagating equivalent
    let indent = trimmed.get(..leading_len).unwrap_or("");
    // TODO(rs-cleanup): silencing at line 315 — review and replace with Result-propagating equivalent
    let body = trimmed.get(leading_len..).unwrap_or("");
    // Accept both `/// <sentinel>` and `///<sentinel>` forms; the
    // former is what prettyplease emits when the doc string starts with
    // a leading space, the latter without.
    let after_triple_raw = body.strip_prefix("///")?;
    let after_triple = after_triple_raw
        .strip_prefix(' ')
        // TODO(rs-cleanup): silencing at line 322 — review and replace with Result-propagating equivalent
        .unwrap_or(after_triple_raw);
    let after_prefix = after_triple.strip_prefix(SENTINEL_INNER_PREFIX)?;
    let digits = after_prefix.strip_suffix(SENTINEL_INNER_SUFFIX)?;
    // TODO(rs-cleanup): silencing — malformed index treated as "not a
    // sentinel"; review whether it should hard-error.
    // TODO(rs-cleanup): silencing at line 327 — review and replace with Result-propagating equivalent
    let idx: usize = digits.parse().ok()?;
    Ok(Some((indent, idx)))
}

/// Append a `/* pre-migration (rudzio-migrate): … */` block comment
/// covering `snippet`, preserving the leading `indent` on every line.
fn push_block_comment(out: &mut String, indent: &str, snippet: &str) -> Result<()> {
    out.push_str(indent);
    out.push_str("/* pre-migration (rudzio-migrate):\n");
    let normalized = normalize_snippet_indent(snippet)?;
    for original_line in normalized.lines() {
        if !original_line.is_empty() {
            out.push_str(indent);
            out.push_str(original_line);
        }
        out.push('\n');
    }
    out.push_str(indent);
    out.push_str("*/\n");
    Ok(())
}

/// Re-indent `original` so each line carries `indent` as a prefix. When
/// `indent` is empty, returns the original verbatim (typical case).
fn reindent_block(original: &str, indent: &str) -> Result<String> {
    // The impl's source already carries its own leading indent relative
    // to its position in the input file; the placeholder's `indent`
    // from the post-prettyplease output is typically "". When `indent`
    // is empty, just return the original as-is so we don't mangle
    // interior whitespace. Otherwise prepend `indent` on each line —
    // rare case, mostly defensive.
    if indent.is_empty() {
        return Ok(original.to_owned());
    }
    // Real error path: capacity overflow on a pathologically long indent
    // surfaces as an error instead of a silently-saturated allocation.
    let extra = indent
        .len()
        .checked_mul(8)
        .context("reindent_block: indent capacity overflow")?;
    let capacity = original
        .len()
        .checked_add(extra)
        .context("reindent_block: capacity overflow")?;
    let mut out = String::with_capacity(capacity);
    for (idx, line) in original.lines().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(indent);
        out.push_str(line);
    }
    if original.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Append every `test_context` bridge impl that targets `path` into a
/// single string. Empty when no plan owns this file.
//
// NOTE(rs-cleanup): kept `-> String` (not Result). Pure iteration +
// string concatenation over already-resolved plans — there is no
// fallible operation here, so a Result return would be an empty wrapper
// that `clippy::unnecessary_wraps` correctly rejects. Adding a fake
// error path would violate the no-fake-solutions rule. The fn carries
// no non-result-fns hazard, so it is not even flagged.
fn render_bridge_for_file(path: &Path, resolver: &TestContextResolver) -> Result<String> {
    let mut out = String::new();
    for plan in resolver.plans.values() {
        if plan.impl_file == path {
            out.push_str(&test_context::render_bridge_impls(plan));
        }
    }
    Ok(out)
}

/// Walk `file.items` and nested module bodies. For every `Item::Impl`
/// that contains any `ImplItem::Verbatim`, capture the impl's original
/// source text via span byte-range and replace the item with a
/// placeholder const that prettyplease can render. Returns one
/// `SalvagedImpl` per replacement.
fn salvage_verbatim_impls(file: &mut syn::File, source: &str) -> Result<Vec<SalvagedImpl>> {
    let mut out = Vec::new();
    salvage_verbatim_impls_in_items(&mut file.items, source, &mut out)?;
    Ok(out)
}

/// Recursive helper for [`salvage_verbatim_impls`] that handles a
/// single items list (top-level or nested module body).
fn salvage_verbatim_impls_in_items(
    items: &mut [syn::Item],
    source: &str,
    out: &mut Vec<SalvagedImpl>,
) -> Result<()> {
    for item in items.iter_mut() {
        let impl_has_verbatim = if let syn::Item::Impl(impl_block) = item {
            impl_block
                .items
                .iter()
                .any(|impl_item| matches!(impl_item, syn::ImplItem::Verbatim(_)))
        } else {
            false
        };
        if impl_has_verbatim {
            let original_source = capture_item_source(item, source)?
                // TODO(rs-cleanup): silencing — falls back to the raw token
                // stream when no source span was captured. Review whether a
                // missing span should hard-error instead.
                .unwrap_or_else(|| quote::ToTokens::to_token_stream(item).to_string());
            let index = out.len();
            out.push(SalvagedImpl {
                index,
                original_source,
            });
            let ident_str = format!("__RUDZIO_MIGRATE_VERBATIM_IMPL_PLACEHOLDER_{index}");
            let ident = syn::Ident::new(&ident_str, proc_macro2::Span::call_site());
            let placeholder: syn::Item = syn::parse_quote! {
                #[allow(dead_code, non_camel_case_types)]
                const #ident: () = ();
            };
            *item = placeholder;
            continue;
        }
        if let syn::Item::Mod(module) = item
            && let Some((_, inner)) = &mut module.content
        {
            salvage_verbatim_impls_in_items(inner, source, out)?;
        }
    };
    Ok(())
}

/// Place the generated bridge / suite types right before the first
/// `#[::rudzio::suite(`, `#[rudzio::suite(`, `#[::rudzio::main]`, or
/// `#[rudzio::main]` line in the file — whichever comes first.
///
/// Falls back to appending at the end if none of those are present
/// (unlikely for a file we touched, but a safe default). Putting the
/// types BEFORE the suite block + fn main keeps the generated diff
/// readable: the user reads the new declarations first, then sees them
/// referenced.
fn splice_bridge_before_first_suite_or_main(output: &str, bridge: &str) -> Result<String> {
    const ANCHORS: &[&str] = &[
        "#[::rudzio::suite(",
        "#[rudzio::suite(",
        "#[::rudzio::main]",
        "#[rudzio::main]",
    ];
    let earliest_anchor = output
        .lines()
        .scan(0_usize, |offset, line| {
            let here = *offset;
            // TODO(rs-cleanup): masking_arith at line 420 — review and replace with Result-propagating equivalent
            *offset = here.saturating_add(line.len()).saturating_add(1);
            Some((here, line))
        })
        .find_map(|(offset, line)| {
            let trimmed = line.trim_start();
            ANCHORS
                .iter()
                .any(|anchor| trimmed.starts_with(anchor))
                .then_some(offset)
        });
    let Some(idx) = earliest_anchor else {
        let mut out = output.to_owned();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(bridge);
        return Ok(out);
    };
    // saturating capacity hint is intentional (over-reserve is harmless).
    let mut out =
        String::with_capacity(output.len().saturating_add(bridge.len()).saturating_add(1));
    // Real error path: `idx` is an anchor byte offset from the line scan;
    // an out-of-range split means the offset bookkeeping is corrupted.
    let prefix = output
        .get(..idx)
        .context("splice_bridge: anchor offset past end of output")?;
    let suffix = output
        .get(idx..)
        .context("splice_bridge: anchor offset past end of output")?;
    out.push_str(prefix);
    out.push_str(bridge);
    if !bridge.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(suffix);
    Ok(out)
}

/// Replace every sentinel doc-comment line with a `/* pre-migration */`
/// block carrying the user's original test fn body.
fn splice_preserved_originals(output: &str, originals: &[String]) -> Result<String> {
    if originals.is_empty() {
        return Ok(output.to_owned());
    }
    let mut out = String::with_capacity(
        output
            .len()
            // TODO(rs-cleanup): masking_arith at line 462 — review and replace with Result-propagating equivalent
            .saturating_add(originals.iter().map(String::len).sum::<usize>())
            // TODO(rs-cleanup): masking_arith at line 463 — review and replace with Result-propagating equivalent
            .saturating_add(256),
    );
    for line in output.split_inclusive('\n') {
        // FIXME(rs-cleanup): callee returns Result, add ?
        if let Some((indent, idx)) = parse_sentinel_line(line)
            && let Some(snippet) = originals.get(idx)
        {
            push_block_comment(&mut out, indent, snippet)?;
            continue;
        }
        out.push_str(line);
    }
    Ok(out)
}

/// After `prettyplease::unparse` renders the tree, each salvaged impl
/// is visible as a line like `const
/// __RUDZIO_MIGRATE_VERBATIM_IMPL_PLACEHOLDER_N: () = ();` (with its
/// `#[allow(...)]` on the line above). This finds the placeholder
/// block and swaps it back for the original impl source text captured
/// earlier.
fn splice_salvaged_verbatim_impls(output: &str, salvaged: &[SalvagedImpl]) -> Result<String> {
    let mut result = output.to_owned();
    for entry in salvaged {
        let const_line = format!(
            "const __RUDZIO_MIGRATE_VERBATIM_IMPL_PLACEHOLDER_{}: () = ();",
            entry.index
        );
        let Some(const_pos) = result.find(&const_line) else {
            continue;
        };
        // Walk backward to grab the leading `#[allow(...)]` attr line
        // that prettyplease emits with the placeholder.
        let block_start = backward_scan_to_attrs(&result, const_pos)?;
        // TODO(rs-cleanup): masking_arith at line 496 — review and replace with Result-propagating equivalent
        let block_end = const_pos.saturating_add(const_line.len());
        // Indentation of the first replaced byte — carry into each
        // line of the spliced-in original so nesting stays intact.
        let indent = indent_at(&result, block_start)?;
        let indented = reindent_block(&entry.original_source, &indent)?;
        result.replace_range(block_start..block_end, &indented);
    }
    Ok(result)
}
