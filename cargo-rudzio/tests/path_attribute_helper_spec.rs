//! Behavioral spec: `#[path = "…"] mod NAME;` declarations on the
//! test-root file are honored by helper discovery (Bug #8B).
//!
//! # Symptom (pre-fix, cargo-rudzio 0.1.1)
//!
//! `discover_test_root_helpers` reads `mod.rs` line by line and
//! delegates to `parse_top_level_mod_decl`, which matches
//! `[pub(…)] mod NAME;` and IGNORES the `#[path = "…"]` attribute
//! immediately above. The helper resolver then falls back to its
//! default sibling lookup (`<dir>/<name>.rs` or
//! `<dir>/<name>/mod.rs`) and either:
//!  - silently skips the decl when no sibling exists at the default
//!    location (so `use crate::<name>::…` inside test files fails
//!    to compile);
//!  - or mounts the WRONG file when a sibling happens to share the
//!    name but lives at the wrong path.
//!
//! Real-world impact: workspaces that share a `common.rs` helper
//! across multiple test binaries via
//! `#[path = "../shared/common.rs"] mod common;` are unsupported.
//!
//! # Behavioral assertion
//!
//! Fixture has a `tests/integration/mod.rs` declaring
//! `#[path = "../shared/common.rs"] mod common;` with the real
//! helper at `tests/shared/common.rs`. There is intentionally
//! NO `tests/integration/common.rs` (the default sibling) so the
//! bug is unambiguous: if `discover_test_root_helpers` honors
//! `#[path]`, the aggregator mounts `tests/shared/common.rs`; if it
//! ignores `#[path]`, the helper is dropped entirely.
//!
//! Read the generated aggregator `Cargo.toml` + `src/` and assert
//! the path attribute target appears in some emitted mount.
//! Pre-fix: it doesn't (helper dropped). Post-fix: it does.

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

/// One-member workspace with a shared helper outside the test-root
/// dir, mounted via `#[path]` from the test root.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
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
    fs::create_dir_all(consumer.join("src"))?;
    fs::create_dir_all(consumer.join("tests").join("integration"))?;
    fs::create_dir_all(consumer.join("tests").join("shared"))?;
    fs::write(
        consumer.join("Cargo.toml"),
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
         path = \"tests/integration/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n",
    )?;
    fs::write(
        consumer.join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    fs::write(
        consumer.join("tests").join("shared").join("common.rs"),
        "pub(crate) const SHARED_TOKEN: () = ();\n",
    )?;
    // The `#[path]` attribute MUST be honored by helper discovery.
    // No `tests/integration/common.rs` exists, so the default
    // sibling lookup falls off the cliff if the attribute is ignored.
    fs::write(
        consumer.join("tests").join("integration").join("mod.rs"),
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         #[path = \"../shared/common.rs\"]\n\
         mod common;\n\
         mod foo;\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;
    fs::write(
        consumer.join("tests").join("integration").join("foo.rs"),
        "use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use crate::common::SHARED_TOKEN;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn shared_helper_visible() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let _: () = SHARED_TOKEN;\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
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

/// Walk the aggregator output directory and concatenate every .rs +
/// Cargo.toml file's text — enough to search for a path-attribute
/// target without caring exactly which file emits the mount.
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

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        plan_from_manifest, read_emitted_sources, rudzio_source_root, with_cargo_offline,
        write_fixture, write_runner,
    };

    #[rudzio::test]
    fn path_attribute_mod_decl_mounts_attribute_target() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;
        let emitted = read_emitted_sources(&out_dir)?;
        // The attribute target `tests/shared/common.rs` must appear
        // SOMEWHERE in the emitted output (as a `#[path]` value, a
        // `path =` Cargo.toml entry, or wherever the fix routes it).
        // Pre-fix: the helper was dropped entirely and this fragment
        // never lands. Use `shared/common.rs` (forward-slash; the
        // generator emits forward slashes even on Windows for `#[path]`).
        anyhow::ensure!(
            emitted.contains("shared/common.rs") || emitted.contains("shared\\common.rs"),
            "aggregator did not mount the `#[path]`-attribute target \
             `tests/shared/common.rs`; helper was dropped. \
             Emitted output:\n{emitted}",
        );
        Ok(())
    }
}
