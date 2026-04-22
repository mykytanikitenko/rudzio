//! Read → parse → mutate → emit pipeline for a single Rust source
//! file.

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};

use crate::backup;
use crate::cli::RuntimeChoice;
use crate::report::Report;
use crate::rewrite::{self, FileRewrite};
use crate::test_context::{self, TestContextResolver};

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
    let source = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
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
        &source,
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
        prettyplease::unparse(&tree)
    } else {
        source.clone()
    };
    if opts.preserve_originals && rewrite.changed {
        output = splice_preserved_originals(&output, &rewrite.original_snippets);
    }
    if !bridge_suffix.is_empty() {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&bridge_suffix);
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
    let mut out = String::with_capacity(output.len() + originals.iter().map(String::len).sum::<usize>() + 256);
    for line in output.split_inclusive('\n') {
        if let Some((indent, idx)) = parse_sentinel_line(line) {
            if let Some(snippet) = originals.get(idx) {
                push_block_comment(&mut out, indent, snippet);
                continue;
            }
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
