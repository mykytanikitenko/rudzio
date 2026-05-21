//! Defensive edge cases for Bug #8 helper discovery.
//!
//! `discover_test_root_helpers` parses each test-binary's root file
//! and emits a re-export for every `mod <name>;` declaration so
//! `use crate::<name>::…` inside sibling test files resolves the
//! same way it does under stock cargo. The base case is covered by
//! `crate_path_under_aggregator_spec`; #8B (`#[path]` attribute) by
//! `path_attribute_helper_spec`; #8C (`main.rs` root) by
//! `main_rs_test_root_helper_spec`. The edge cases this spec covers:
//!
//! - `pub mod common;` (public visibility prefix) is discovered.
//! - `pub(crate) mod common;` (restricted visibility) is discovered.
//! - Multiple helper mods (`mod a; mod b;`) — every helper mounts.
//! - Absolute `#[path]` (not just relative `../`) is honored.
//! - `lib.rs` as test root (extends #8C's `main.rs` coverage).
//!
//! All cases are library-level static assertions on the emitted
//! aggregator source — fast, deterministic, no cargo build needed.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the in-tree rudzio package.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// One-member workspace at `root` whose `consumer/tests/integration/`
/// directory follows `layout`. `mod_decls` is the body of the test
/// root (`mod.rs` or `main.rs` or `lib.rs`) — every `mod X;` it
/// names must have a matching `<X>.rs` under `consumer/tests/integration/`.
///
/// `extra_files` lets callers add helper files (or `shared/` sibling
/// dirs) outside the test root.
fn write_fixture(
    root: &Path,
    rudzio_root: &Path,
    test_root_name: &str,
    test_root_body: &str,
    helper_files: &[(&str, &str)],
    extra_files: &[(&str, &str)],
) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", default-features = false, features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    let consumer = root.join("consumer");
    let integration = consumer.join("tests").join("integration");
    fs::create_dir_all(consumer.join("src"))?;
    fs::create_dir_all(&integration)?;
    fs::write(
        consumer.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"consumer\"\n\
             version = \"0.0.0\"\n\
             edition = \"2024\"\n\
             publish = false\n\
             autotests = false\n\
             \n\
             [lib]\n\
             name = \"consumer\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [[test]]\n\
             name = \"integration\"\n\
             path = \"tests/integration/{test_root_name}\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             rudzio = {{ workspace = true }}\n",
        ),
    )?;
    fs::write(
        consumer.join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    fs::write(integration.join(test_root_name), test_root_body)?;
    for (rel, contents) in helper_files {
        let path = integration.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    for (rel, contents) in extra_files {
        let path = consumer.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    Ok(())
}

/// `CARGO_NET_OFFLINE=true` guard. Same RAII pattern as the other
/// specs in this directory.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII restore.
    struct Restore {
        /// Previous value (`None` = was unset).
        prev: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                #[expect(
                    unsafe_code,
                    reason = "std::env::set_var marked unsafe; guard owns the var"
                )]
                // SAFETY: see other specs.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; guard owns the var"
                )]
                // SAFETY: see other specs.
                unsafe {
                    env::remove_var("CARGO_NET_OFFLINE");
                }
            }
        }
    }
    let prev = env::var("CARGO_NET_OFFLINE").ok();
    let _guard = Restore { prev };
    #[expect(
        unsafe_code,
        reason = "std::env::set_var marked unsafe; guard owns the var"
    )]
    // SAFETY: see Restore::drop.
    unsafe {
        env::set_var("CARGO_NET_OFFLINE", "true");
    }
    body()
}

/// Recursively concatenate every `.rs` + `.toml` under `out_dir`.
fn read_emitted_sources(out_dir: &Path) -> io::Result<String> {
    let mut buf = String::new();
    let mut stack: Vec<PathBuf> = vec![out_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for raw_entry in fs::read_dir(&dir)? {
            let entry = raw_entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if !kind.is_dir()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "rs" || extension == "toml")
                && let Ok(text) = fs::read_to_string(&path)
            {
                buf.push_str(&text);
                buf.push('\n');
            } else {
                // Other file type or non-.rs/.toml — ignored.
            }
        }
    }
    Ok(buf)
}

/// Drive `plan_from_manifest` + `write_runner` against the fixture at
/// `<temp>/Cargo.toml` and return the concatenated emitted source.
fn emit(temp: &Path) -> anyhow::Result<String> {
    let manifest = temp.join("Cargo.toml");
    let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
    let out_dir = plan.default_output_dir();
    write_runner(&plan, &out_dir)?;
    Ok(read_emitted_sources(&out_dir)?)
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{emit, rudzio_source_root, write_fixture};

    /// `pub mod common;` — the leading `pub` visibility must NOT
    /// cause helper discovery to skip the decl. `parse_top_level_mod_decl`
    /// strips `pub `, `pub(crate) `, and `pub(super) ` prefixes.
    #[rudzio::test]
    fn pub_visibility_prefix_on_helper_mod_is_discovered() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(
            temp.path(),
            &rudzio_root,
            "mod.rs",
            "// rudzio_test marker\n\
             pub mod common;\n\
             #[rudzio::main]\nfn main() {}\n",
            &[("common.rs", "pub const PUB_TOKEN: () = ();\n")],
            &[],
        )?;
        let emitted = emit(temp.path())?;
        anyhow::ensure!(
            emitted.contains("common.rs"),
            "pub-prefixed helper mod must be mounted at the aggregator root:\n{emitted}",
        );
        Ok(())
    }

    /// `pub(crate) mod common;` — the restricted visibility form.
    #[rudzio::test]
    fn pub_crate_visibility_prefix_on_helper_mod_is_discovered() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(
            temp.path(),
            &rudzio_root,
            "mod.rs",
            "// rudzio_test marker\n\
             pub(crate) mod common;\n\
             #[rudzio::main]\nfn main() {}\n",
            &[("common.rs", "pub(crate) const PC_TOKEN: () = ();\n")],
            &[],
        )?;
        let emitted = emit(temp.path())?;
        anyhow::ensure!(
            emitted.contains("common.rs"),
            "pub(crate)-prefixed helper mod must be mounted:\n{emitted}",
        );
        Ok(())
    }

    /// Two helper mods declared back-to-back — both discovered.
    #[rudzio::test]
    fn multiple_helper_mods_all_discovered() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(
            temp.path(),
            &rudzio_root,
            "mod.rs",
            "// rudzio_test marker\n\
             mod common;\n\
             mod fixtures;\n\
             #[rudzio::main]\nfn main() {}\n",
            &[
                ("common.rs", "pub const COMMON_TOKEN: () = ();\n"),
                ("fixtures.rs", "pub const FIXTURES_TOKEN: () = ();\n"),
            ],
            &[],
        )?;
        let emitted = emit(temp.path())?;
        anyhow::ensure!(
            emitted.contains("common.rs") && emitted.contains("fixtures.rs"),
            "both helper mods must be mounted at the aggregator root:\n{emitted}",
        );
        Ok(())
    }

    /// `#[path = "/abs/path/helper.rs"]` with an absolute path. The
    /// fixture renders the absolute path at write-time, so this is a
    /// best-effort defense against discoverers that only handle
    /// relative `../foo`.
    #[rudzio::test]
    fn absolute_path_attribute_on_helper_mod_is_discovered() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        let shared = temp.path().join("consumer").join("tests").join("shared");
        let abs_helper = shared.join("common.rs");
        write_fixture(
            temp.path(),
            &rudzio_root,
            "mod.rs",
            &format!(
                "// rudzio_test marker\n\
                 #[path = {abs:?}]\n\
                 mod common;\n\
                 #[rudzio::main]\nfn main() {{}}\n",
                abs = abs_helper.display().to_string(),
            ),
            &[],
            &[("tests/shared/common.rs", "pub const ABS_TOKEN: () = ();\n")],
        )?;
        let emitted = emit(temp.path())?;
        anyhow::ensure!(
            emitted.contains("shared/common.rs") || emitted.contains("shared\\common.rs"),
            "absolute `#[path]` target `tests/shared/common.rs` must be mounted:\n{emitted}",
        );
        Ok(())
    }

    /// `lib.rs` as test-root entry — extends #8C beyond `main.rs`.
    /// Some workspaces use `tests/integration/lib.rs` for shared
    /// integration roots; helper discovery should treat it the same
    /// way as `main.rs`.
    #[rudzio::test]
    fn lib_rs_test_root_discovers_sibling_helpers() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(
            temp.path(),
            &rudzio_root,
            "lib.rs",
            "// rudzio_test marker\n\
             mod common;\n\
             #[rudzio::main]\nfn main() {}\n",
            &[("common.rs", "pub const LIB_TOKEN: () = ();\n")],
            &[],
        )?;
        let emitted = emit(temp.path())?;
        // Helper basename must appear in some `#[path]` mount —
        // could be the source path (`tests/integration/common.rs`)
        // or the proxy path (`<out>/src/proxies/.../common.rs`)
        // depending on Bug #10 scoping.
        anyhow::ensure!(
            emitted.contains("/common.rs") || emitted.contains("\\common.rs"),
            "`lib.rs`-rooted test binary must discover sibling helpers:\n{emitted}",
        );
        Ok(())
    }
}
