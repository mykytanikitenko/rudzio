//! File backups written immediately before the tool overwrites an
//! existing file.
//!
//! The suffix is intentionally long and specific
//! (`.backup_before_migration_to_rudzio`) to avoid collisions with any
//! convention the user's repo might already use, and to make the
//! intent obvious at a glance in the file listing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(any(test, rudzio_test))]
use rudzio::common::context::{Suite, Test};
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
#[cfg(any(test, rudzio_test))]
use rudzio::runtime::{async_std, compio, embassy};

/// Suffix appended to every original file path to derive its backup
/// path. Long and specific by design.
pub const SUFFIX: &str = ".backup_before_migration_to_rudzio";

/// Outcome of a backup attempt for a single file.
#[derive(Debug)]
#[non_exhaustive]
pub enum Outcome {
    /// A backup at the destination already existed; nothing was written.
    AlreadyExists(PathBuf),
    /// A new backup was created at the destination.
    Created(PathBuf),
}

impl Outcome {
    /// The destination path of the backup, regardless of whether it was
    /// freshly created or already present.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::AlreadyExists(path) | Self::Created(path) => path,
        }
    }
}

/// The path a backup of `original` would live at — i.e., `original`
/// with [`SUFFIX`] appended.
#[inline]
#[must_use]
pub fn path_for(original: &Path) -> PathBuf {
    let mut bytes = original.as_os_str().to_owned();
    bytes.push(SUFFIX);
    PathBuf::from(bytes)
}

/// Copy `original` to its sibling `.backup_before_migration_to_rudzio`.
///
/// Idempotent: if a backup already exists at the destination this
/// returns [`Outcome::AlreadyExists`] and leaves the existing backup in
/// place (first copy wins; the idea is that repeated runs against a
/// dirty tree with leftover backups from a prior run are already
/// blocked by preflight, so getting here implies the user deliberately
/// kept the old backup).
///
/// # Errors
///
/// Returns the underlying `io::Error` if [`fs::copy`] fails.
#[inline]
pub fn copy_before_write(original: &Path) -> io::Result<Outcome> {
    let dest = path_for(original);
    if dest.exists() {
        return Ok(Outcome::AlreadyExists(dest));
    }
    let _bytes_copied = fs::copy(original, &dest)?;
    Ok(Outcome::Created(dest))
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
])]
#[cfg(any(test, rudzio_test))]
mod tests {
    use super::{Path, Test, path_for};

    #[rudzio::test]
    async fn backup_suffix_appends_to_path(_ctx: &Test) -> anyhow::Result<()> {
        let path = Path::new("/tmp/foo/bar.rs");
        anyhow::ensure!(
            path_for(path).as_path()
                == Path::new("/tmp/foo/bar.rs.backup_before_migration_to_rudzio"),
            "path_for did not append the expected suffix to {}",
            path.display(),
        );
        Ok(())
    }

    #[rudzio::test]
    async fn backup_suffix_applies_to_cargo_toml(_ctx: &Test) -> anyhow::Result<()> {
        let path = Path::new("/tmp/foo/Cargo.toml");
        anyhow::ensure!(
            path_for(path).as_path()
                == Path::new("/tmp/foo/Cargo.toml.backup_before_migration_to_rudzio"),
            "path_for did not append the expected suffix to {}",
            path.display(),
        );
        Ok(())
    }
}
