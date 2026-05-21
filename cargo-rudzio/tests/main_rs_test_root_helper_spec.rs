//! Behavioral spec: a `[[test]]` whose `path` resolves to `main.rs`
//! (or `lib.rs`) is treated as a helper-discovery root, same as
//! `mod.rs` (Bug #8C).
//!
//! # Symptom (pre-fix, cargo-rudzio 0.1.1)
//!
//! `discover_test_root_helpers` early-returns when the test file's
//! `file_name()` isn't exactly `mod.rs`:
//!
//! ```text
//! if test_file.file_name() != Some("mod.rs") { return Vec::new(); }
//! ```
//!
//! After Bug #7's fix removed the implicit `mod.rs` requirement,
//! real-world workspaces also use `tests/<name>/main.rs` as the
//! `[[test]]` entry. For those binaries, helper discovery returns
//! empty and `use crate::common::…` in sibling test files fails to
//! compile because the helper file never gets mounted at the
//! aggregator crate root.
//!
//! # Behavioral assertion
//!
//! Fixture: one `[[test]]` with `path = "tests/integration/main.rs"`
//! (NOT `mod.rs`), declaring `mod common; mod foo;` inside `main.rs`,
//! with a sibling `common.rs` carrying a uniquely-named constant.
//! Drive `plan_from_manifest` + `write_runner`; assert the emitted
//! aggregator mounts a `common` helper that references the
//! fixture's `tests/integration/common.rs`. Pre-fix: helper is
//! dropped entirely. Post-fix: helper appears in the emitted output.

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

/// One-member workspace whose test binary is rooted at `main.rs`
/// (not `mod.rs`), with a sibling `common.rs` helper.
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
         path = \"tests/integration/main.rs\"\n\
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
        consumer.join("tests").join("integration").join("main.rs"),
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         mod common;\n\
         mod foo;\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;
    // Unique constant name so the static check can pinpoint THIS
    // fixture's helper (not some other test's `common.rs`).
    fs::write(
        consumer.join("tests").join("integration").join("common.rs"),
        "pub(crate) const MAIN_RS_ROOT_TOKEN: () = ();\n",
    )?;
    fs::write(
        consumer.join("tests").join("integration").join("foo.rs"),
        "use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20use crate::common::MAIN_RS_ROOT_TOKEN;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn helper_under_main_rs_root_visible() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let _: () = MAIN_RS_ROOT_TOKEN;\n\
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

/// Recursively concatenate every `.rs` + `.toml` file under
/// `out_dir`. Used to search for the helper's filename without
/// caring which file in the emitted aggregator actually mounts it.
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
                // Other file type (symlink, socket, etc.) or non-.rs/.toml
                // file — ignored for source-content scanning.
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
    fn main_rs_test_root_discovers_sibling_helpers() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;
        let emitted = read_emitted_sources(&out_dir)?;
        // The sibling helper `common.rs` must appear as a `#[path]`
        // target somewhere in the emitted output. May be the source
        // path (`tests/integration/common.rs`) or the proxy path
        // (`<out>/src/proxies/consumer__main/common.rs`) depending
        // on whether Bug #10's per-binary scoping is active. Either
        // shape proves the helper was discovered. Pre-fix
        // `discover_test_root_helpers` early-returned for `main.rs`
        // and the file never reached any mount; the substring would
        // be absent.
        anyhow::ensure!(
            emitted.contains("/common.rs") || emitted.contains("\\common.rs"),
            "aggregator did not mount the sibling helper under a \
             `main.rs` test root; `discover_test_root_helpers` \
             refused the non-`mod.rs` entry. Emitted output:\n{emitted}",
        );
        Ok(())
    }
}
