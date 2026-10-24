//! Regression guard for Bug #9: the README must never document the
//! nonexistent `--format json` runner flag.
//!
//! The runner's `--format` accepts exactly `pretty` and `terse` (see
//! `src/config.rs::handle_presentation_flag`, which maps every non-`terse`
//! value to `Format::Pretty`). `cargo rudzio test --format json` was a
//! stale README example pointing at a flag that does not exist — a user
//! following it gets silently-`pretty` output, not JSON. The fix removes
//! the example so the README's `--format` contract (`--format
//! <pretty|terse>`) matches the runner's actual surface.
//!
//! This guard keeps the two in lockstep: it reads the committed README
//! and asserts the exact stale tokens are absent, so a future edit that
//! re-introduces `--format json` (or `--format=json`) fails the suite
//! with the reason spelled out.

use std::path::Path;

use rudzio::common::context::Suite;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;

/// Repo root = parent of `cargo-rudzio/` (where this spec's package
/// manifest lives). `env!("CARGO_MANIFEST_DIR")` resolves to
/// `<repo>/cargo-rudzio`, so its parent is the root that holds
/// `README.md`.
fn repo_root() -> anyhow::Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cargo-rudzio/tests must live under the workspace root"))
}

#[rudzio::suite([(runtime = FuturesThreadPool::new, suite = Suite, test = Test)])]
mod tests {
    use std::fs::read_to_string;

    use super::repo_root;

    #[rudzio::test]
    fn readme_does_not_document_format_json() -> anyhow::Result<()> {
        let readme = read_to_string(repo_root()?.join("README.md"))?;
        anyhow::ensure!(
            !readme.contains("--format json") && !readme.contains("--format=json"),
            "README must not document `--format json` / `--format=json`: the runner's `--format` \
             accepts only `pretty` and `terse` (Bug #9). Remove the stale example and keep the \
             `--format <pretty|terse>` contract.",
        );
        Ok(())
    }
}
