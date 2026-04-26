//! Summary report: what got migrated, what got warned, what got
//! skipped. Printed at the end of a run to stdout. Progress lines
//! during the run go through `progress(...)`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct Report {
    pub files_touched: Vec<PathBuf>,
    pub tests_converted: usize,
    pub cargo_toml_edits: Vec<PathBuf>,
    pub backups_created: Vec<PathBuf>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone)]
pub struct Warning {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub message: String,
    /// Pre-rewrite byte span (offset, length) of the attribute / fn
    /// that produced this warning. Together with `source` this lets
    /// the summary underline the exact snippet. `None` means the
    /// warning is location-less (e.g. Cargo.toml edit failures that
    /// don't point at a specific line).
    pub span: Option<(usize, usize)>,
    /// Pre-rewrite file bytes, captured at warning time so the summary
    /// renders what the user actually wrote — not the post-rewrite
    /// output whose line numbers no longer line up.
    pub source: Option<Arc<String>>,
}

impl Report {
    #[must_use] 
    pub const fn new() -> Self {
        Self {
            files_touched: Vec::new(),
            tests_converted: 0,
            cargo_toml_edits: Vec::new(),
            backups_created: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn warn(&mut self, file: PathBuf, line: Option<usize>, message: impl Into<String>) {
        self.warnings.push(Warning {
            file,
            line,
            message: message.into(),
            span: None,
            source: None,
        });
    }

    pub fn warn_with_span(
        &mut self,
        file: PathBuf,
        line: usize,
        byte_offset: usize,
        byte_len: usize,
        source: Arc<String>,
        message: impl Into<String>,
    ) {
        self.warnings.push(Warning {
            file,
            line: Some(line),
            message: message.into(),
            span: Some((byte_offset, byte_len)),
            source: Some(source),
        });
    }

    pub fn touched(&mut self, path: PathBuf) {
        self.files_touched.push(path);
    }

    pub fn backed_up(&mut self, path: PathBuf) {
        self.backups_created.push(path);
    }

    pub fn cargo_edit(&mut self, path: PathBuf) {
        self.cargo_toml_edits.push(path);
    }

    pub const fn add_converted(&mut self, count: usize) {
        self.tests_converted = self.tests_converted.saturating_add(count);
    }

    pub fn print_summary<W: Write>(&self, mut out: W) -> std::io::Result<()> {
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
            for w in &self.warnings {
                render_warning(&mut out, w)?;
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
}

pub fn progress<W: Write>(mut out: W, msg: &str) -> std::io::Result<()> {
    writeln!(out, "{msg}")?;
    out.flush()
}

fn render_warning<W: Write>(mut out: W, w: &Warning) -> std::io::Result<()> {
    // Header line: `path:line: message` (or `path: message` when location-less).
    match w.line {
        Some(line) => writeln!(out, "  {}:{}: {}", w.file.display(), line, w.message)?,
        None => writeln!(out, "  {}: {}", w.file.display(), w.message)?,
    }
    // If we captured the offending span and source, render a one-line
    // snippet with a caret underline beneath it. No fancy graphics,
    // just enough context for the user to find the spot.
    let (Some((offset, len)), Some(source)) = (w.span, &w.source) else {
        return Ok(());
    };
    let len = len.max(1);
    let line_start = source[..offset.min(source.len())]
        .rfind('\n')
        .map_or(0, |i| i.saturating_add(1));
    let line_end = source[line_start..]
        .find('\n')
        .map_or(source.len(), |i| line_start.saturating_add(i));
    let snippet = &source[line_start..line_end];
    let col = offset.saturating_sub(line_start);
    writeln!(out, "    | {snippet}")?;
    let pad = " ".repeat(col);
    let underline = "^".repeat(len.min(line_end.saturating_sub(offset).max(1)));
    writeln!(out, "    | {pad}{underline}")?;
    Ok(())
}
