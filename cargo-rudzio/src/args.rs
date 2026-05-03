//! Cargo-style argument parsing for `cargo rudzio test`.
//!
//! Lives in the library (rather than `main.rs`) so integration tests can
//! drive each parser directly with synthetic argv slices and assert the
//! consumed-vs-forwarded split is correct for every flag spelling.

use anyhow::{Result, bail};

/// Pull `-p <name>` / `-p=<name>` / `--package <name>` / `--package=<name>` out of `args`.
///
/// Returns the collected package names and the args list with those
/// entries removed (so downstream parsing — path restriction, runner
/// forwarding — only ever sees what's left).
///
/// Mirrors cargo's own `-p` semantics: repeatable, takes one name per
/// occurrence, name match is exact against the Cargo package name
/// (hyphenated form). Without this consumption, `-p` would land in
/// the aggregator's argv where the rudzio runner would warn about an
/// unrecognised flag and treat the package name as a positional
/// substring filter that almost never matches a fully-qualified test.
///
/// # Errors
///
/// Returns an error when `-p` / `--package` is the last arg with no
/// following value, or when the equals form supplies an empty name.
#[inline]
pub fn parse_package_filters(args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut packages = Vec::new();
    let mut remaining = Vec::new();
    let mut idx = 0_usize;
    while let Some(arg) = args.get(idx) {
        if arg == "-p" || arg == "--package" {
            let next_idx = idx.saturating_add(1_usize);
            let value = args
                .get(next_idx)
                .ok_or_else(|| anyhow::anyhow!("`{arg}` requires a package name"))?;
            packages.push(value.clone());
            idx = next_idx.saturating_add(1_usize);
        } else if let Some(value) = arg
            .strip_prefix("-p=")
            .or_else(|| arg.strip_prefix("--package="))
        {
            if value.is_empty() {
                bail!("`{arg}` requires a non-empty package name");
            }
            packages.push(value.to_owned());
            idx = idx.saturating_add(1_usize);
        } else {
            remaining.push(arg.clone());
            idx = idx.saturating_add(1_usize);
        }
    }
    Ok((packages, remaining))
}
