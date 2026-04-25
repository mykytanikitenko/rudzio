//! Summary report: what got migrated, what got warned, what got
//! skipped. Printed at the end of a run to stdout. Progress lines
//! during the run go through `progress(...)`.

use std::io::Write;
use std::path::PathBuf;

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
}

impl Report {
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

    pub fn add_converted(&mut self, count: usize) {
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
                match w.line {
                    Some(line) => writeln!(out, "  {}:{}: {}", w.file.display(), line, w.message)?,
                    None => writeln!(out, "  {}: {}", w.file.display(), w.message)?,
                }
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
