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
        let salvaged = salvage_verbatim_impls(&mut tree, &source);
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
            rendered = splice_salvaged_verbatim_impls(&rendered, &salvaged);
        }
        rendered
    } else {
        source.to_string()
    };
    if opts.preserve_originals && rewrite.changed {
        output = splice_preserved_originals(&output, &rewrite.original_snippets);
    }
    if !bridge_suffix.is_empty() {
        output = splice_bridge_before_first_suite_or_main(&output, &bridge_suffix);
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
fn backward_scan_to_attrs(text: &str, const_pos: usize) -> usize {
    // `const_pos` points at `c` of the placeholder const; the newline
    // immediately before it (at const_pos-1) terminates the attribute
    // line we want to grab. Start earliest at the beginning of the
    // const line itself, then walk further back over each preceding
    // `#[...]` line.
    let mut earliest = const_pos;
    loop {
        // Exclude the newline right before `earliest`, otherwise
        // `rfind('\n')` returns the same newline forever.
        let search_end = earliest.saturating_sub(1);
        let prefix = text.get(..search_end).unwrap_or("");
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
        let line_start = prev_newline.saturating_add(1);
        let line = text.get(line_start..search_end).unwrap_or("");
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[") {
            earliest = line_start;
        } else {
            break;
        }
    }
    earliest
}

/// Compute the byte range of an item from the source by walking its
/// span. Uses `proc_macro2`'s `span-locations` feature; the range
/// covers the outermost attribute through the closing brace.
fn capture_item_source(item: &syn::Item, source: &str) -> Option<String> {
    let start = if let syn::Item::Impl(impl_block) = item {
        impl_block
            .attrs
            .iter()
            .map(|attr| attr.span().byte_range().start)
            .min()
            .unwrap_or_else(|| impl_block.impl_token.span.byte_range().start)
    } else {
        item.span().byte_range().start
    };
    let end = item.span().byte_range().end;
    if start < end && end <= source.len() {
        source.get(start..end).map(str::to_owned)
    } else {
        None
    }
}

/// Leading-spaces character count for `text`.
fn indent_at(text: &str, pos: usize) -> String {
    let prefix = text.get(..pos).unwrap_or("");
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx.saturating_add(1));
    let line = text.get(line_start..pos).unwrap_or("");
    line.chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect()
}

/// Count of leading ASCII space bytes in `text`.
fn leading_space_count(text: &str) -> usize {
    text.bytes().take_while(|byte| *byte == b' ').count()
}

/// Strip the common leading whitespace shared across non-empty lines so
/// the original's relative indentation is preserved while we place it
/// under the target indent.
fn normalize_snippet_indent(snippet: &str) -> String {
    let lines: Vec<&str> = snippet.lines().collect();
    if lines.len() <= 1 {
        return snippet.to_owned();
    }
    // The first line starts at the attribute byte, so it has no leading
    // indent; ignore it when computing common indent.
    let min_indent: usize = lines
        .iter()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| leading_space_count(line))
        .min()
        .unwrap_or(0);
    let mut out = String::with_capacity(snippet.len());
    for (idx, line) in lines.iter().enumerate() {
        if idx == 0 {
            out.push_str(line);
        } else if line.trim().is_empty() {
            // Blank line: leave it as a bare newline so we preserve
            // separation without trying to dedent into nothing.
        } else {
            let skip = leading_space_count(line).min(min_indent);
            out.push_str(line.get(skip..).unwrap_or(""));
        }
        out.push('\n');
    }
    out
}

/// Match a pretty-printed line against the placeholder sentinel form
/// emitted by the rewriter; return `(indent, idx)` if it matches.
fn parse_sentinel_line(line: &str) -> Option<(&str, usize)> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let leading_len = trimmed.len().saturating_sub(trimmed.trim_start().len());
    let indent = trimmed.get(..leading_len).unwrap_or("");
    let body = trimmed.get(leading_len..).unwrap_or("");
    // Accept both `/// <sentinel>` and `///<sentinel>` forms; the
    // former is what prettyplease emits when the doc string starts with
    // a leading space, the latter without.
    let after_triple_raw = body.strip_prefix("///")?;
    let after_triple = after_triple_raw
        .strip_prefix(' ')
        .unwrap_or(after_triple_raw);
    let after_prefix = after_triple.strip_prefix(SENTINEL_INNER_PREFIX)?;
    let digits = after_prefix.strip_suffix(SENTINEL_INNER_SUFFIX)?;
    let idx: usize = digits.parse().ok()?;
    Some((indent, idx))
}

/// Append a `/* pre-migration (rudzio-migrate): … */` block comment
/// covering `snippet`, preserving the leading `indent` on every line.
fn push_block_comment(out: &mut String, indent: &str, snippet: &str) {
    out.push_str(indent);
    out.push_str("/* pre-migration (rudzio-migrate):\n");
    let normalized = normalize_snippet_indent(snippet);
    for original_line in normalized.lines() {
        if !original_line.is_empty() {
            out.push_str(indent);
            out.push_str(original_line);
        }
        out.push('\n');
    }
    out.push_str(indent);
    out.push_str("*/\n");
}

/// Re-indent `original` so each line carries `indent` as a prefix. When
/// `indent` is empty, returns the original verbatim (typical case).
fn reindent_block(original: &str, indent: &str) -> String {
    // The impl's source already carries its own leading indent relative
    // to its position in the input file; the placeholder's `indent`
    // from the post-prettyplease output is typically "". When `indent`
    // is empty, just return the original as-is so we don't mangle
    // interior whitespace. Otherwise prepend `indent` on each line —
    // rare case, mostly defensive.
    if indent.is_empty() {
        return original.to_owned();
    }
    let mut out = String::with_capacity(
        original
            .len()
            .saturating_add(indent.len().saturating_mul(8)),
    );
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
    out
}

/// Append every `test_context` bridge impl that targets `path` into a
/// single string. Empty when no plan owns this file.
fn render_bridge_for_file(path: &Path, resolver: &TestContextResolver) -> String {
    let mut out = String::new();
    for plan in resolver.plans.values() {
        if plan.impl_file == path {
            out.push_str(&test_context::render_bridge_impls(plan));
        }
    }
    out
}

/// Walk `file.items` and nested module bodies. For every `Item::Impl`
/// that contains any `ImplItem::Verbatim`, capture the impl's original
/// source text via span byte-range and replace the item with a
/// placeholder const that prettyplease can render. Returns one
/// `SalvagedImpl` per replacement.
fn salvage_verbatim_impls(file: &mut syn::File, source: &str) -> Vec<SalvagedImpl> {
    let mut out = Vec::new();
    salvage_verbatim_impls_in_items(&mut file.items, source, &mut out);
    out
}

/// Recursive helper for [`salvage_verbatim_impls`] that handles a
/// single items list (top-level or nested module body).
fn salvage_verbatim_impls_in_items(
    items: &mut [syn::Item],
    source: &str,
    out: &mut Vec<SalvagedImpl>,
) {
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
            let original_source = capture_item_source(item, source)
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
            salvage_verbatim_impls_in_items(inner, source, out);
        }
    }
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
fn splice_bridge_before_first_suite_or_main(output: &str, bridge: &str) -> String {
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
    earliest_anchor.map_or_else(
        || {
            let mut out = output.to_owned();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(bridge);
            out
        },
        |idx| {
            let mut out =
                String::with_capacity(output.len().saturating_add(bridge.len()).saturating_add(1));
            out.push_str(output.get(..idx).unwrap_or(""));
            out.push_str(bridge);
            if !bridge.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(output.get(idx..).unwrap_or(""));
            out
        },
    )
}

/// Replace every sentinel doc-comment line with a `/* pre-migration */`
/// block carrying the user's original test fn body.
fn splice_preserved_originals(output: &str, originals: &[String]) -> String {
    if originals.is_empty() {
        return output.to_owned();
    }
    let mut out = String::with_capacity(
        output
            .len()
            .saturating_add(originals.iter().map(String::len).sum::<usize>())
            .saturating_add(256),
    );
    for line in output.split_inclusive('\n') {
        if let Some((indent, idx)) = parse_sentinel_line(line)
            && let Some(snippet) = originals.get(idx)
        {
            push_block_comment(&mut out, indent, snippet);
            continue;
        }
        out.push_str(line);
    }
    out
}

/// After `prettyplease::unparse` renders the tree, each salvaged impl
/// is visible as a line like `const
/// __RUDZIO_MIGRATE_VERBATIM_IMPL_PLACEHOLDER_N: () = ();` (with its
/// `#[allow(...)]` on the line above). This finds the placeholder
/// block and swaps it back for the original impl source text captured
/// earlier.
fn splice_salvaged_verbatim_impls(output: &str, salvaged: &[SalvagedImpl]) -> String {
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
        let block_start = backward_scan_to_attrs(&result, const_pos);
        let block_end = const_pos.saturating_add(const_line.len());
        // Indentation of the first replaced byte — carry into each
        // line of the spliced-in original so nesting stays intact.
        let indent = indent_at(&result, block_start);
        let indented = reindent_block(&entry.original_source, &indent);
        result.replace_range(block_start..block_end, &indented);
    }
    result
}
