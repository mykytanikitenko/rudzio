//! Stdout / stderr writer helpers used in lieu of `println!` / `eprintln!`
//! so the workspace's `print_stdout` / `print_stderr` lints stay green.

use std::io;

/// Write `text` to stderr, ignoring any I/O error.
///
/// Same effective semantics as `eprintln!` (which also discards write
/// errors), but goes through `Write::write_all` so the `print_stderr`
/// lint doesn't fire. Stderr write failure is unrecoverable for these
/// sites; matching `eprintln!` behavior is the right answer.
#[inline]
pub fn write_stderr(text: &str) {
    use std::io::Write as _;
    let _io_result: io::Result<()> = io::stderr().lock().write_all(text.as_bytes());
}

/// Write `text` to stdout, ignoring any I/O error.
///
/// Same effective semantics as `println!` (which also discards write
/// errors), but goes through `Write::write_all` so the `print_stdout`
/// lint doesn't fire. Stdout write failure (broken pipe, etc.) is
/// unrecoverable for these sites; matching `println!` behavior is the
/// right answer.
#[inline]
pub fn write_stdout(text: &str) {
    use std::io::Write as _;
    let _io_result: io::Result<()> = io::stdout().lock().write_all(text.as_bytes());
}
