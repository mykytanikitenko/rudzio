//! Preflight gates that must pass before the tool touches any file:
//!   1. The target path is inside a git repo.
//!   2. The working tree is clean (no staged / unstaged / untracked).
//!   3. The user has typed the acknowledgement phrase verbatim.
//!
//! All three are hard blocks with no bypass flag.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::phrase::ACK_PHRASE;

#[derive(Debug)]
pub enum PreflightError {
    NotAGitRepo(PathBuf),
    DirtyTree,
    WrongAcknowledgement,
    Io(io::Error),
}

impl core::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAGitRepo(p) => write!(f, "not inside a git repository: {}", p.display()),
            Self::DirtyTree => f.write_str("working tree is not clean"),
            Self::WrongAcknowledgement => f.write_str("acknowledgement did not match"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for PreflightError {}

impl From<io::Error> for PreflightError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub const DIRTY_TREE_MESSAGE: &str = "\
rudzio-migrate: refusing to run because the working tree has uncommitted changes.

This tool is not going to do any magic. It will try, on a best-effort
basis, to convert every test in this repository into a rudzio test and
\u{2014} if you let it \u{2014} generate a shared runner entry point.

Actions may be destructive by accident. The tool does not guarantee
that the generated or modified code compiles, that your tests still
pass, or that the conversion preserves their original meaning. It is
not going to save your project or make your test suite magically
better. Take its output as a direction and eliminate most of the
manual work; review every diff.

To proceed: commit or stash your changes, then re-run.
";

pub const INTRO_MESSAGE: &str = "\
rudzio-migrate: best-effort test migration.

This tool is not going to do any magic. It will try, on a best-effort
basis, to convert every test in this repository into a rudzio test and
\u{2014} if you let it \u{2014} generate a shared runner entry point.

Actions may be destructive by accident. The tool does not guarantee
that the generated or modified code compiles, that your tests still
pass, or that the conversion preserves their original meaning. It is
not going to save your project or make your test suite magically
better. Take its output as a direction and eliminate most of the
manual work; review every diff.
";

/// Find the git repo root for `path`. Returns `Err(NotAGitRepo)` if
/// `git rev-parse --show-toplevel` fails.
pub fn git_root(path: &Path) -> Result<PathBuf, PreflightError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err(PreflightError::NotAGitRepo(path.to_path_buf()));
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if s.is_empty() {
        return Err(PreflightError::NotAGitRepo(path.to_path_buf()));
    }
    Ok(PathBuf::from(s))
}

/// Returns `Ok(())` if the working tree at `repo_root` has no staged,
/// unstaged, or untracked changes; `Err(DirtyTree)` otherwise.
pub fn require_clean_tree(repo_root: &Path) -> Result<(), PreflightError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain"])
        .output()?;
    if !output.status.success() {
        return Err(PreflightError::DirtyTree);
    }
    if !output.stdout.is_empty() {
        return Err(PreflightError::DirtyTree);
    }
    Ok(())
}

/// Blocks reading stdin until the user types (or pipes) a line matching
/// `ACK_PHRASE` verbatim.
pub fn require_acknowledgement<R, W>(mut reader: R, mut writer: W) -> Result<(), PreflightError>
where
    R: BufRead,
    W: Write,
{
    writeln!(
        writer,
        "\nType the following sentence exactly, then press Enter, to continue:"
    )?;
    writeln!(writer, "\n  {ACK_PHRASE}\n")?;
    writer.flush()?;

    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(PreflightError::WrongAcknowledgement);
    }
    let trimmed = strip_one_newline(&line);
    if trimmed == ACK_PHRASE {
        Ok(())
    } else {
        Err(PreflightError::WrongAcknowledgement)
    }
}

fn strip_one_newline(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn ack_phrase_round_trip_exact() {
        let mut input = Cursor::new(format!("{ACK_PHRASE}\n").into_bytes());
        let mut output = Vec::<u8>::new();
        require_acknowledgement(&mut input, &mut output).expect("ack should match");
    }

    #[test]
    fn ack_phrase_reject_when_corrected() {
        let mut input = Cursor::new(
            b"I am not an idiot and understand what I am doing in most cases at least\n".to_vec(),
        );
        let mut output = Vec::<u8>::new();
        assert!(matches!(
            require_acknowledgement(&mut input, &mut output),
            Err(PreflightError::WrongAcknowledgement)
        ));
    }

    #[test]
    fn ack_phrase_reject_on_empty_stdin() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        assert!(matches!(
            require_acknowledgement(&mut input, &mut output),
            Err(PreflightError::WrongAcknowledgement)
        ));
    }

    #[test]
    fn ack_phrase_accepts_crlf_trailer() {
        let mut input = Cursor::new(format!("{ACK_PHRASE}\r\n").into_bytes());
        let mut output = Vec::<u8>::new();
        require_acknowledgement(&mut input, &mut output).expect("ack should match");
    }
}
