//! Preflight gates that must pass before the tool touches any file.
//!
//! Three hard blocks, in order:
//!   1. The target path is inside a git repo.
//!   2. The working tree is clean (no staged / unstaged / untracked).
//!   3. The user has typed the acknowledgement phrase verbatim.
//!
//! All three are non-bypassable.

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(any(test, rudzio_test))]
use rudzio::common::context::{Suite, Test};
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::tokio::Multithread;

use crate::phrase::ACK_PHRASE;

/// The `git status --porcelain` rejection text printed verbatim when
/// the working tree isn't clean.
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

/// Intro message printed before the acknowledgement prompt.
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

/// Reasons preflight may refuse to proceed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Failure {
    /// `git status --porcelain` reported staged, unstaged, or
    /// untracked changes.
    DirtyTree,
    /// Underlying I/O error while invoking `git` or reading stdin.
    Io(io::Error),
    /// `git rev-parse --show-toplevel` failed for the target path.
    NotAGitRepo(PathBuf),
    /// The user did not type the acknowledgement phrase verbatim.
    WrongAcknowledgement,
}

impl fmt::Display for Failure {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirtyTree => f.write_str("working tree is not clean"),
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::NotAGitRepo(path) => {
                write!(f, "not inside a git repository: {}", path.display())
            }
            Self::WrongAcknowledgement => f.write_str("acknowledgement did not match"),
        }
    }
}

impl StdError for Failure {}

impl From<io::Error> for Failure {
    #[inline]
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Find the git repo root for `path`.
///
/// # Errors
///
/// Returns [`Failure::NotAGitRepo`] when `git rev-parse --show-toplevel`
/// exits non-zero or yields an empty path; returns [`Failure::Io`] if
/// the `git` invocation itself fails.
#[inline]
pub fn git_root(path: &Path) -> Result<PathBuf, Failure> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err(Failure::NotAGitRepo(path.to_path_buf()));
    }
    let trimmed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if trimmed.is_empty() {
        return Err(Failure::NotAGitRepo(path.to_path_buf()));
    }
    Ok(PathBuf::from(trimmed))
}

/// Block reading stdin until the user types (or pipes) a line matching
/// [`ACK_PHRASE`] verbatim.
///
/// # Errors
///
/// Returns [`Failure::WrongAcknowledgement`] if stdin is empty or the
/// trimmed line doesn't match; returns [`Failure::Io`] for read or
/// write failures on the supplied streams.
#[inline]
pub fn require_acknowledgement<R, W>(mut reader: R, mut writer: W) -> Result<(), Failure>
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
    let bytes_read = reader.read_line(&mut line)?;
    if bytes_read == 0 {
        return Err(Failure::WrongAcknowledgement);
    }
    let trimmed = strip_one_newline(&line);
    if trimmed == ACK_PHRASE {
        Ok(())
    } else {
        Err(Failure::WrongAcknowledgement)
    }
}

/// Returns `Ok(())` if the working tree at `repo_root` has no staged,
/// unstaged, or untracked changes.
///
/// # Errors
///
/// Returns [`Failure::DirtyTree`] when `git status --porcelain` exits
/// non-zero or reports any change; returns [`Failure::Io`] if the
/// `git` invocation itself fails.
#[inline]
pub fn require_clean_tree(repo_root: &Path) -> Result<(), Failure> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain"])
        .output()?;
    if !output.status.success() {
        return Err(Failure::DirtyTree);
    }
    if !output.stdout.is_empty() {
        return Err(Failure::DirtyTree);
    }
    Ok(())
}

/// Strip a single trailing `\r\n` or `\n` from `input`. Used to
/// normalise the acknowledgement line read from stdin.
fn strip_one_newline(input: &str) -> &str {
    input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .unwrap_or(input)
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
#[cfg(any(test, rudzio_test))]
mod tests {
    use std::io::Cursor;

    use super::{ACK_PHRASE, Failure, Test, require_acknowledgement};

    #[rudzio::test]
    async fn ack_phrase_accepts_crlf_trailer(_ctx: &Test) -> anyhow::Result<()> {
        let mut input = Cursor::new(format!("{ACK_PHRASE}\r\n").into_bytes());
        let mut output = Vec::<u8>::new();
        require_acknowledgement(&mut input, &mut output)
            .map_err(|err| anyhow::anyhow!("ack should match: {err}"))?;
        Ok(())
    }

    #[rudzio::test]
    async fn ack_phrase_reject_on_empty_stdin(_ctx: &Test) -> anyhow::Result<()> {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        anyhow::ensure!(
            matches!(
                require_acknowledgement(&mut input, &mut output),
                Err(Failure::WrongAcknowledgement)
            ),
            "empty stdin should yield WrongAcknowledgement",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn ack_phrase_reject_when_corrected(_ctx: &Test) -> anyhow::Result<()> {
        let mut input = Cursor::new(
            b"I am not an idiot and understand what I am doing in most cases at least\n".to_vec(),
        );
        let mut output = Vec::<u8>::new();
        anyhow::ensure!(
            matches!(
                require_acknowledgement(&mut input, &mut output),
                Err(Failure::WrongAcknowledgement)
            ),
            "corrected phrase should be rejected",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn ack_phrase_round_trip_exact(_ctx: &Test) -> anyhow::Result<()> {
        let mut input = Cursor::new(format!("{ACK_PHRASE}\n").into_bytes());
        let mut output = Vec::<u8>::new();
        require_acknowledgement(&mut input, &mut output)
            .map_err(|err| anyhow::anyhow!("ack should match: {err}"))?;
        Ok(())
    }
}
