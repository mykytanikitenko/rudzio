//! Read → parse → mutate → emit pipeline for a single Rust source
//! file.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use crate::backup;
use crate::cli::RuntimeChoice;
use crate::report::Report;
use crate::rewrite::{self, FileRewrite};
use crate::test_context::{self, TestContextResolver};

#[derive(Debug)]
pub struct EmitOptions<'a> {
    pub default_runtime: RuntimeChoice,
    pub preserve_originals: bool,
    pub dry_run: bool,
    pub test_contexts: &'a TestContextResolver,
}

pub fn process_file(
    path: &Path,
    opts: &EmitOptions<'_>,
    report: &mut Report,
) -> Result<Option<FileRewrite>> {
    let source: Arc<String> =
        Arc::new(fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?);
    let mut tree: syn::File = match syn::parse_file(&source) {
        Ok(t) => t,
        Err(err) => {
            report.warn(
                path.to_path_buf(),
                Some(err.span().start().line),
                format!("syn parse failed: {err}; skipping file"),
            );
            return Ok(None);
        }
    };

    let rewrite = rewrite::rewrite_file(
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
        // Replace each offending `Item::Impl` with a
        // single-line placeholder constant whose name encodes the
        // index, capturing the impl's original source text
        // separately. After `prettyplease::unparse` succeeds, the
        // placeholder line gets spliced out and the original impl
        // text is stitched in. The rest of the file (the tests
        // the user actually cares about) unparses normally.
        let salvaged = salvage_verbatim_impls(&mut tree, &source);
        // Last-resort safety net: if prettyplease still panics on
        // some shape we didn't normalise, skip the whole rewrite
        // with a warning rather than aborting the run.
        if let Ok(mut s) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            prettyplease::unparse(&tree)
        })) {
            if !salvaged.is_empty() {
                s = splice_salvaged_verbatim_impls(&s, &salvaged);
            }
            s
        } else {
            report.warn(
                path.to_path_buf(),
                None,
                "prettyplease::unparse panicked on this file (likely an ImplItem::Verbatim \u{2014} bodyless `fn X(&self);` from a macro such as `ambassador::delegate_to_remote_methods`); skipping the rewrite, original file left untouched",
            );
            return Ok(None);
        }
    } else {
        (*source).clone()
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
        if matches!(bak, backup::BackupOutcome::Created(_)) {
            report.backed_up(bak.path().to_path_buf());
        }
        fs::write(path, &output).with_context(|| format!("writing {}", path.display()))?;
        report.touched(path.to_path_buf());
    }

    Ok(Some(rewrite))
}

/// Place the generated bridge / suite types right before the first
/// `#[::rudzio::suite(`, `#[rudzio::suite(`, `#[::rudzio::main]`,
/// or `#[rudzio::main]` line in the file — whichever comes first.
/// Falls back to appending at the end if none of those are present
/// (unlikely for a file we touched, but a safe default). Putting
/// the types BEFORE the suite block + fn main keeps the generated
/// diff readable: the user reads the new declarations first, then
/// sees them referenced.
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
                .any(|a| trimmed.starts_with(a))
                .then_some(offset)
        });
    if let Some(idx) = earliest_anchor {
        let mut out = String::with_capacity(output.len() + bridge.len() + 1);
        out.push_str(&output[..idx]);
        out.push_str(bridge);
        if !bridge.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&output[idx..]);
        out
    } else {
        let mut out = output.to_owned();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(bridge);
        out
    }
}

/// An `Item::Impl` that prettyplease can't render (because it
/// carries an `ImplItem::Verbatim`) replaced in-tree by a
/// single-line placeholder const. The `original_source` is the
/// exact bytes from the input file; after prettyplease succeeds
/// on the rest of the tree, the placeholder line gets spliced
/// back out and the original source gets stitched in — so the
/// impl survives the round-trip with formatting intact.
struct SalvagedImpl {
    /// Index embedded in the placeholder's ident, used to find
    /// the placeholder line in prettyplease's output.
    index: usize,
    /// Exact byte-range text of the original impl from the input
    /// source, including its outer attributes.
    original_source: String,
}

/// Walk `file.items` and nested module bodies. For every
/// `Item::Impl` that contains any `ImplItem::Verbatim`, capture
/// the impl's original source text via span byte-range and
/// replace the item with a placeholder const that prettyplease
/// can render. Returns one `SalvagedImpl` per replacement.
fn salvage_verbatim_impls(file: &mut syn::File, source: &str) -> Vec<SalvagedImpl> {
    let mut out = Vec::new();
    salvage_verbatim_impls_in_items(&mut file.items, source, &mut out);
    out
}

fn salvage_verbatim_impls_in_items(
    items: &mut Vec<syn::Item>,
    source: &str,
    out: &mut Vec<SalvagedImpl>,
) {
    for item in items.iter_mut() {
        let impl_has_verbatim = match item {
            syn::Item::Impl(i) => i
                .items
                .iter()
                .any(|ii| matches!(ii, syn::ImplItem::Verbatim(_))),
            _ => false,
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
        if let syn::Item::Mod(m) = item
            && let Some((_, inner)) = &mut m.content {
                salvage_verbatim_impls_in_items(inner, source, out);
            }
    }
}

/// Compute the byte range of an item from the source by walking
/// its span. Uses `proc_macro2`'s `span-locations` feature; the
/// range covers the outermost attribute through the closing brace.
fn capture_item_source(item: &syn::Item, source: &str) -> Option<String> {
    use syn::spanned::Spanned as _;
    let start = match item {
        syn::Item::Impl(i) => i
            .attrs
            .iter()
            .map(|a| a.span().byte_range().start)
            .min()
            .unwrap_or_else(|| i.impl_token.span.byte_range().start),
        _ => item.span().byte_range().start,
    };
    let end = item.span().byte_range().end;
    (start < end && end <= source.len()).then(|| source[start..end].to_owned())
}

/// After `prettyplease::unparse` renders the tree, each salvaged
/// impl is visible as a line like `const
/// __RUDZIO_MIGRATE_VERBATIM_IMPL_PLACEHOLDER_N: () = ();` (with
/// its `#[allow(...)]` on the line above). This finds the
/// placeholder block and swaps it back for the original impl
/// source text captured earlier.
fn splice_salvaged_verbatim_impls(output: &str, salvaged: &[SalvagedImpl]) -> String {
    let mut result = output.to_owned();
    for s in salvaged {
        let const_line = format!(
            "const __RUDZIO_MIGRATE_VERBATIM_IMPL_PLACEHOLDER_{}: () = ();",
            s.index
        );
        let Some(const_pos) = result.find(&const_line) else {
            continue;
        };
        // Walk backward to grab the leading `#[allow(...)]` attr
        // line that prettyplease emits with the placeholder.
        let block_start = backward_scan_to_attrs(&result, const_pos);
        let block_end = const_pos + const_line.len();
        // Indentation of the first replaced byte — carry into each
        // line of the spliced-in original so nesting stays intact.
        let indent = indent_at(&result, block_start);
        let indented = reindent_block(&s.original_source, indent);
        result.replace_range(block_start..block_end, &indented);
    }
    result
}

/// Walk backward from `const_pos` skipping blank lines and `#[...]`
/// attribute lines until we hit something else; return the
/// earliest-attribute byte offset (start of the line the first
/// encountered attribute lives on).
fn backward_scan_to_attrs(text: &str, const_pos: usize) -> usize {
    // `const_pos` points at `c` of the placeholder const; the
    // newline immediately before it (at const_pos-1) terminates
    // the attribute line we want to grab. Start earliest at the
    // beginning of the const line itself, then walk further back
    // over each preceding `#[...]` line.
    let mut earliest = const_pos;
    loop {
        // Exclude the newline right before `earliest`, otherwise
        // `rfind('\n')` returns the same newline forever.
        let search_end = earliest.saturating_sub(1);
        let Some(prev_newline) = text[..search_end].rfind('\n') else {
            // Reached start of file — nothing left to consume.
            if search_end == 0 {
                break;
            }
            let line = text[..search_end].trim_start();
            if line.starts_with("#[") {
                earliest = 0;
            }
            break;
        };
        let line_start = prev_newline + 1;
        let line = &text[line_start..search_end];
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[") {
            earliest = line_start;
        } else {
            break;
        }
    }
    earliest
}

fn indent_at(text: &str, pos: usize) -> String {
    let prefix = &text[..pos];
    let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
    let line = &text[line_start..pos];
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn reindent_block(original: &str, indent: String) -> String {
    // The impl's source already carries its own leading indent
    // relative to its position in the input file; the placeholder's
    // `indent` from the post-prettyplease output is typically "".
    // When `indent` is empty, just return the original as-is so we
    // don't mangle interior whitespace. Otherwise prepend `indent`
    // on each line — rare case, mostly defensive.
    if indent.is_empty() {
        return original.to_owned();
    }
    let mut out = String::with_capacity(original.len() + indent.len() * 8);
    for (i, line) in original.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&indent);
        out.push_str(line);
    }
    if original.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn render_bridge_for_file(path: &Path, resolver: &TestContextResolver) -> String {
    let mut out = String::new();
    for plan in resolver.plans.values() {
        if plan.impl_file == path {
            out.push_str(&test_context::render_bridge_impls(plan));
        }
    }
    out
}

fn splice_preserved_originals(output: &str, originals: &[String]) -> String {
    if originals.is_empty() {
        return output.to_owned();
    }
    let mut out = String::with_capacity(
        output.len() + originals.iter().map(String::len).sum::<usize>() + 256,
    );
    for line in output.split_inclusive('\n') {
        if let Some((indent, idx)) = parse_sentinel_line(line)
            && let Some(snippet) = originals.get(idx) {
                push_block_comment(&mut out, indent, snippet);
                continue;
            }
        out.push_str(line);
    }
    out
}

const SENTINEL_INNER_PREFIX: &str = "__RUDZIO_MIGRATE_ORIGINAL_PLACEHOLDER_";
const SENTINEL_INNER_SUFFIX: &str = "__";

fn parse_sentinel_line(line: &str) -> Option<(&str, usize)> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let leading_len = trimmed.len() - trimmed.trim_start().len();
    let indent = &trimmed[..leading_len];
    let body = &trimmed[leading_len..];
    // Accept both `/// <sentinel>` and `///<sentinel>` forms; the former
    // is what prettyplease emits when the doc string starts with a
    // leading space, the latter without.
    let after_triple = body.strip_prefix("///")?;
    let after_triple = after_triple.strip_prefix(' ').unwrap_or(after_triple);
    let after_prefix = after_triple.strip_prefix(SENTINEL_INNER_PREFIX)?;
    let digits = after_prefix.strip_suffix(SENTINEL_INNER_SUFFIX)?;
    let idx: usize = digits.parse().ok()?;
    Some((indent, idx))
}

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

/// Strips the common leading whitespace shared across non-empty lines
/// so the original's relative indentation is preserved while we place
/// it under the target indent.
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
        .filter(|l| !l.trim().is_empty())
        .map(|l| leading_space_count(l))
        .min()
        .unwrap_or(0);
    let mut out = String::with_capacity(snippet.len());
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(line);
        } else if !line.trim().is_empty() {
            let skip = leading_space_count(line).min(min_indent);
            out.push_str(&line[skip..]);
        }
        out.push('\n');
    }
    out
}

fn leading_space_count(s: &str) -> usize {
    s.bytes().take_while(|b| *b == b' ').count()
}
