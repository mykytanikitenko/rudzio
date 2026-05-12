//! Summary report: what got migrated, what got warned, what got
//! skipped.
//!
//! Printed at the end of a run to stdout. Progress lines during the run
//! go through [`progress`].

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// Aggregate counters and warning list collected during a run.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Report {
    /// Cargo manifests modified (for example to add `linkme` deps).
    pub backups_created: Vec<PathBuf>,
    /// Per-Cargo.toml edits issued.
    pub cargo_toml_edits: Vec<PathBuf>,
    /// Source files where the rewriter actually wrote a change.
    pub files_touched: Vec<PathBuf>,
    /// Number of test functions translated.
    pub tests_converted: usize,
    /// Manual-follow-up warnings the user needs to act on.
    pub warnings: Vec<Warning>,
}

/// One warning entry shown in the final summary.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Warning {
    /// File the warning applies to.
    pub file: PathBuf,
    /// Line number, when known. `None` for location-less warnings
    /// (e.g. Cargo.toml edit failures that don't point at a row).
    pub line: Option<usize>,
    /// Human-readable warning text.
    pub message: String,
    /// Pre-rewrite file bytes, captured at warning time so the summary
    /// renders what the user actually wrote — not the post-rewrite
    /// output whose line numbers no longer line up.
    pub source: Option<Arc<str>>,
    /// Pre-rewrite byte span (offset, length) of the attribute / fn
    /// that produced this warning. Together with `source` this lets
    /// the summary underline the exact snippet. `None` means the
    /// warning is location-less.
    pub span: Option<(usize, usize)>,
}

impl Report {
    /// Bump the converted-test counter by `count`. Saturating so that
    /// the report can't overflow if a run tries hard enough.
    #[inline]
    pub const fn add_converted(&mut self, count: usize) {
        self.tests_converted = self.tests_converted.saturating_add(count);
    }

    /// Record that `path` was backed up (i.e. a sibling
    /// `.backup_before_migration_to_rudzio` was created for it).
    #[inline]
    pub fn backed_up(&mut self, path: PathBuf) {
        self.backups_created.push(path);
    }

    /// Record that `path` (a `Cargo.toml`) had edits applied.
    #[inline]
    pub fn cargo_edit(&mut self, path: PathBuf) {
        self.cargo_toml_edits.push(path);
    }

    /// Construct an empty report.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            backups_created: Vec::new(),
            cargo_toml_edits: Vec::new(),
            files_touched: Vec::new(),
            tests_converted: 0,
            warnings: Vec::new(),
        }
    }

    /// Render the final summary into `out`.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if any of the writes or the
    /// final flush fails.
    #[inline]
    pub fn print_summary<W: Write>(&self, mut out: W) -> io::Result<()> {
        writeln!(out)?;
        writeln!(out, "== rudzio-migrate summary ==")?;
        writeln!(out, "Files touched:       {}", self.files_touched.len())?;
        writeln!(out, "Tests converted:     {}", self.tests_converted)?;
        writeln!(
            out,
            "Cargo.toml edits:    {} files",
            self.cargo_toml_edits.len()
        )?;
        writeln!(
            out,
            "Backups created:     {}  (*.backup_before_migration_to_rudzio)",
            self.backups_created.len()
        )?;
        writeln!(out, "Warnings:            {}", self.warnings.len())?;
        if !self.warnings.is_empty() {
            writeln!(out)?;
            writeln!(out, "Warnings (need manual follow-up):")?;
            for warning in &self.warnings {
                render_warning(&mut out, warning)?;
            }
        }
        writeln!(out)?;
        writeln!(out, "Next steps:")?;
        writeln!(
            out,
            "  1. git diff   \u{2014} review every change. This tool is not magic."
        )?;
        writeln!(
            out,
            "  2. cargo check --tests   \u{2014} if anything does not compile, the diff"
        )?;
        writeln!(
            out,
            "     is your friend; the conversion is mechanical and localized."
        )?;
        writeln!(out, "  3. Address the warnings (file:line list above).")?;
        writeln!(out, "  4. Once satisfied, delete the backups:")?;
        writeln!(
            out,
            "       find . -name '*.backup_before_migration_to_rudzio' -delete"
        )?;
        writeln!(
            out,
            "     Or add the glob to .gitignore and keep them around during review."
        )?;
        out.flush()?;
        Ok(())
    }

    /// Record that `path` was touched by the rewriter.
    #[inline]
    pub fn touched(&mut self, path: PathBuf) {
        self.files_touched.push(path);
    }

    /// Push a location-bearing warning without a source snippet.
    #[inline]
    pub fn warn<S: Into<String>>(&mut self, file: PathBuf, line: Option<usize>, message: S) {
        self.warnings.push(Warning {
            file,
            line,
            message: message.into(),
            source: None,
            span: None,
        });
    }

    /// Push a warning with a source snippet so the summary can
    /// underline the offending span.
    #[inline]
    pub fn warn_with_span<S: Into<String>>(
        &mut self,
        file: PathBuf,
        line: usize,
        byte_offset: usize,
        byte_len: usize,
        source: Arc<str>,
        message: S,
    ) {
        self.warnings.push(Warning {
            file,
            line: Some(line),
            message: message.into(),
            source: Some(source),
            span: Some((byte_offset, byte_len)),
        });
    }
}

/// Print `msg` as a one-line progress notice on `out`.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] from the write or flush.
#[inline]
pub fn progress<W: Write>(mut out: W, msg: &str) -> io::Result<()> {
    writeln!(out, "{msg}")?;
    out.flush()
}

/// Render a single warning entry: header line plus an optional
/// snippet-with-caret block when a source span was captured.
fn render_warning<W: Write>(mut out: W, warning: &Warning) -> io::Result<()> {
    // Header line: `path:line: message` (or `path: message` when location-less).
    match warning.line {
        Some(line) => writeln!(
            out,
            "  {}:{}: {}",
            warning.file.display(),
            line,
            warning.message,
        )?,
        None => writeln!(out, "  {}: {}", warning.file.display(), warning.message)?,
    }
    // Without a captured span + source, we're done.
    let (Some((offset, raw_len)), Some(source)) = (warning.span, &warning.source) else {
        return Ok(());
    };
    let span_len = raw_len.max(1);
    let scan_end = offset.min(source.len());
    let line_start = source
        .get(..scan_end)
        .and_then(|prefix| prefix.rfind('\n'))
        .map_or(0, |idx| idx.saturating_add(1));
    let tail = source.get(line_start..).unwrap_or("");
    let line_end = tail
        .find('\n')
        .map_or(source.len(), |idx| line_start.saturating_add(idx));
    let snippet = source.get(line_start..line_end).unwrap_or("");
    let col = offset.saturating_sub(line_start);
    writeln!(out, "    | {snippet}")?;
    let pad = " ".repeat(col);
    let caret_count = span_len.min(line_end.saturating_sub(offset).max(1));
    let underline = "^".repeat(caret_count);
    writeln!(out, "    | {pad}{underline}")?;
    Ok(())
}
