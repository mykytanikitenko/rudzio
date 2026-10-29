// Deterministic content fingerprint of a crate's build inputs, shared by
// `build.rs` (which stamps it into the binary at compile time) and
// `cargo-rudzio-test-helpers` (which recomputes it at runtime to prove the
// binary matches the current sources). Both sites `include!` this exact
// file, so the stamp and the check can never drift apart.
//
// # Why content, not git HEAD
//
// A binary is stale only when it was built from *different source bytes*
// than are present now. Git HEAD is a proxy for that: two commits can move
// HEAD without touching any `cargo-rudzio` source (empty harness commits,
// doc-only changes), and the binary stays byte-for-byte identical. Comparing
// the binary's stamped HEAD against the live HEAD flags those builds as
// stale — a false positive. Hashing the source bytes that actually determine
// the binary's output makes a HEAD move with unchanged sources stay
// legitimately fresh, and still rejects a binary whose own source diverged.
//
// # Scope
//
// The fingerprint covers the crate's own inputs (`src/` recursively,
// `build.rs`, `Cargo.toml`). Dependency sources are excluded on purpose:
// cargo's own fingerprint already rebuilds the binary whenever any
// dependency changes, so a cargo-managed binary cannot be stale with respect
// to its dependencies. The freshness guard exists to catch a *foreign or
// stale artifact* whose own source diverged, which this fingerprint detects
// exactly.

/// Module wrapping the fingerprint so `include!`-ing this file never
/// collides with the including crate's own `use` statements or item names.
#[expect(
    clippy::inline_modules,
    reason = "shared via `include!` into `build.rs` and `cargo-rudzio-test-helpers`; the module wrapper namespaces items so they never collide with each including crate's own names"
)]
pub mod source_fingerprint {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// rustc-env key the fingerprint is stamped under.
    pub const ENV: &str = "CARGO_RUDZIO_BUILD_SOURCE_HASH";

    /// Sentinel reported when no source content is readable, e.g. when the
    /// build script runs standalone against an empty temp dir (the
    /// `stamp_resolution_spec` compiles `build.rs` with `rustc` directly).
    pub const UNKNOWN: &str = "unknown";

    /// Names, relative to the manifest dir, whose content determines the
    /// crate's compiled output. `src` is walked recursively.
    const SOURCE_ENTRIES: &[&str] = &["src", "build.rs", "Cargo.toml"];

    /// FNV-1a offset basis.
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    /// FNV-1a prime.
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Compute the hex-encoded fingerprint of `manifest_dir`'s build inputs.
    ///
    /// Records are the sorted relative paths; each contributes its path
    /// bytes, a `0` separator, its content bytes, and a trailing `0`
    /// separator to an FNV-1a running hash. The explicit framing makes the
    /// hash unambiguous (no path/content boundary collisions), and FNV-1a is
    /// fully specified so the value is stable across rustc versions and
    /// across the two include sites. Returns [`UNKNOWN`] when nothing is
    /// readable.
    #[must_use]
    #[inline]
    pub fn of(manifest_dir: &Path) -> String {
        let mut files = Vec::new();
        for entry in SOURCE_ENTRIES {
            collect(manifest_dir, Path::new(entry), &mut files);
        }
        files.sort();

        let mut hash = FNV_OFFSET;
        let mut counted = 0_usize;
        for rel in files {
            let full = manifest_dir.join(&rel);
            let Ok(content) = fs::read(&full) else {
                continue;
            };
            hash = fold(hash, rel.to_string_lossy().as_bytes());
            hash = fold(hash, &[0]);
            hash = fold(hash, &content);
            hash = fold(hash, &[0]);
            counted = counted.saturating_add(1);
        }

        if counted == 0 {
            return UNKNOWN.to_owned();
        }
        format!("{hash:016x}")
    }

    /// Fold `bytes` into the running FNV-1a `hash`.
    fn fold(mut hash: u64, bytes: &[u8]) -> u64 {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    /// Recursively collect source files under `root`/`rel` as paths relative
    /// to `root`. Directories are traversed; unreadable or absent entries are
    /// skipped so a standalone build script against a bare temp dir still
    /// yields a well-defined (possibly empty) set.
    fn collect(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
        let full = root.join(rel);
        if full.is_file() {
            out.push(rel.to_path_buf());
            return;
        }
        if !full.is_dir() {
            return;
        }
        let Ok(entries) = fs::read_dir(&full) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            if path.is_dir() {
                collect(root, relative, out);
            } else {
                out.push(relative.to_path_buf());
            }
        }
    }
}
