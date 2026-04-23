//! File backups written immediately before the tool overwrites an
//! existing file. The suffix is intentionally long and specific
//! (`.backup_before_migration_to_rudzio`) to avoid collisions with any
//! convention the user's repo might already use, and to make the
//! intent obvious at a glance in the file listing.
use std::fs;
use std::path::{Path, PathBuf};
pub const BACKUP_SUFFIX: &str = ".backup_before_migration_to_rudzio";
pub fn backup_path(original: &Path) -> PathBuf {
    let mut s = original.as_os_str().to_owned();
    s.push(BACKUP_SUFFIX);
    PathBuf::from(s)
}
/// Copy `original` to its sibling `.backup_before_migration_to_rudzio`.
/// Idempotent: if a backup already exists at the destination this
/// returns `Ok(false)` and leaves the existing backup in place (first
/// copy wins; the idea is that repeated runs against a dirty tree with
/// leftover backups from a prior run are already blocked by preflight,
/// so getting here implies the user deliberately kept the old backup).
pub fn copy_before_write(original: &Path) -> std::io::Result<BackupOutcome> {
    let dest = backup_path(original);
    if dest.exists() {
        return Ok(BackupOutcome::AlreadyExists(dest));
    }
    let _bytes_copied = fs::copy(original, &dest)?;
    Ok(BackupOutcome::Created(dest))
}
#[derive(Debug)]
pub enum BackupOutcome {
    Created(PathBuf),
    AlreadyExists(PathBuf),
}
impl BackupOutcome {
    pub fn path(&self) -> &Path {
        match self {
            Self::Created(p) | Self::AlreadyExists(p) => p,
        }
    }
}
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
#[cfg(test)]
mod tests {
    use super::*;
    use ::rudzio::common::context::Test;
    /* pre-migration (rudzio-migrate):
    #[test]
    fn backup_suffix_appends_to_path() {
        let p = Path::new("/tmp/foo/bar.rs");
        assert_eq!(
            backup_path(p),
            PathBuf::from("/tmp/foo/bar.rs.backup_before_migration_to_rudzio")
        );
    }
    */
    #[::rudzio::test]
    async fn backup_suffix_appends_to_path(_ctx: &Test) -> ::anyhow::Result<()> {
        let p = Path::new("/tmp/foo/bar.rs");
        assert_eq!(
            backup_path(p),
            PathBuf::from("/tmp/foo/bar.rs.backup_before_migration_to_rudzio")
        );
        ::core::result::Result::Ok(())
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn backup_suffix_applies_to_cargo_toml() {
        let p = Path::new("/tmp/foo/Cargo.toml");
        assert_eq!(
            backup_path(p),
            PathBuf::from("/tmp/foo/Cargo.toml.backup_before_migration_to_rudzio")
        );
    }
    */
    #[::rudzio::test]
    async fn backup_suffix_applies_to_cargo_toml(_ctx: &Test) -> ::anyhow::Result<()> {
        let p = Path::new("/tmp/foo/Cargo.toml");
        assert_eq!(
            backup_path(p),
            PathBuf::from("/tmp/foo/Cargo.toml.backup_before_migration_to_rudzio")
        );
        ::core::result::Result::Ok(())
    }
}
