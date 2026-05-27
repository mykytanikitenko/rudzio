//! Defensive edge cases for Bug #11 double execution.
//!
//! Base spec (`double_execution_spec`) asserts a single test source
//! file is `#[path]`-mounted exactly once across `main.rs` + `tests.rs`.
//! The mount-once invariant must hold across these edges:
//!
//! - **Multi-file binary**: every test file is mounted exactly once.
//!   Failure mode: prefix-mangling regression that emits a per-file
//!   duplicate.
//! - **Helper file containing `#[rudzio::test]`**: the helper is
//!   mounted at the aggregator's crate root (via the Bug #8 fix)
//!   AND is reachable from the test root's mod tree. The Bug #10
//!   fix strips the helper's `mod <name>;` decl from the proxy mod
//!   so the helper's `#[rudzio::test]` registers only once, not
//!   twice.
//! - **`mod.rs` (test root) carrying `#[rudzio::test]`**: the root
//!   itself is mounted once. Failure mode would be a regression
//!   that mounts the proxy root in both `main.rs` and `tests.rs`.
//! - **Cross-binary identical-stem files**: `a/foo.rs` + `b/foo.rs`
//!   each get mounted independently (so 2 total mentions). Failure
//!   mode: dedup keyed only on stem (not binary scope) would
//!   collapse them.

use std::env;
use std::fmt::{Result as FmtResult, Write as _};
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

/// Workspace + consumer skeleton with one `[[test]]` entry. Returns
/// the test-root directory so the caller writes whatever shape it needs.
fn write_single_binary_workspace(root: &Path, rudzio_root: &Path) -> io::Result<PathBuf> {
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
    let test_root_dir = consumer.join("tests").join("integration");
    fs::create_dir_all(consumer.join("src"))?;
    fs::create_dir_all(&test_root_dir)?;
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
    Ok(test_root_dir)
}

/// Two-binary workspace; returns both test-root dirs.
fn write_two_binary_workspace(root: &Path, rudzio_root: &Path) -> io::Result<(PathBuf, PathBuf)> {
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
    let first_test_dir = consumer.join("tests").join("a");
    let second_test_dir = consumer.join("tests").join("b");
    fs::create_dir_all(consumer.join("src"))?;
    fs::create_dir_all(&first_test_dir)?;
    fs::create_dir_all(&second_test_dir)?;
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
         name = \"a\"\n\
         path = \"tests/a/mod.rs\"\n\
         harness = false\n\
         \n\
         [[test]]\n\
         name = \"b\"\n\
         path = \"tests/b/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n",
    )?;
    fs::write(
        consumer.join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    Ok((first_test_dir, second_test_dir))
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

/// Run the aggregator emit pipeline and return aggregator main.rs +
/// every bridge's `src/lib.rs` as a single concatenated string for
/// substring counting.
///
/// Under v2 test/helper proxy mounts (`#[path = "proxies/<id>/foo.rs"]
/// mod __rudzio_test_<id>;` etc.) live in each bridged member's
/// generated `src/lib.rs` rather than in a single aggregator
/// `tests.rs`. The mount-uniqueness assertions therefore need to scan
/// every bridge's lib.rs.
fn emit_combined(temp: &Path) -> anyhow::Result<String> {
    let manifest = temp.join("Cargo.toml");
    let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
    let out_dir = plan.default_output_dir();
    write_runner(&plan, &out_dir)?;
    let main_rs = fs::read_to_string(out_dir.join("src").join("main.rs"))?;
    let mut combined = format!("--- main.rs ---\n{main_rs}");
    let members_dir = out_dir.join("members");
    if let Ok(entries) = fs::read_dir(&members_dir) {
        for entry in entries.flatten() {
            let bridge_dir = entry.path();
            let lib_rs = bridge_dir.join("src").join("lib.rs");
            if let Ok(content) = fs::read_to_string(&lib_rs) {
                let bridge_name = bridge_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?");
                let _written: FmtResult = writeln!(
                    combined,
                    "\n--- members/{bridge_name}/src/lib.rs ---\n{content}"
                );
            }
        }
    }
    Ok(combined)
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        emit_combined, fs, rudzio_source_root, write_single_binary_workspace,
        write_two_binary_workspace,
    };

    /// Two test files in the same binary — each mounted exactly
    /// once across main.rs + tests.rs.
    #[rudzio::test]
    fn multi_file_binary_mounts_each_file_once() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        let test_root_dir = write_single_binary_workspace(temp.path(), &rudzio_root)?;
        fs::write(
            test_root_dir.join("mod.rs"),
            "// rudzio_test marker\n\
             mod foo;\n\
             mod bar;\n\
             #[rudzio::main]\nfn main() {}\n",
        )?;
        fs::write(
            test_root_dir.join("foo.rs"),
            "#[rudzio::suite([(runtime = ::rudzio::runtime::tokio::Multithread::new, \
             suite = ::rudzio::common::context::Suite, \
             test = ::rudzio::common::context::Test)])]\n\
             mod inner { #[rudzio::test] fn foo_test() {} }\n",
        )?;
        fs::write(
            test_root_dir.join("bar.rs"),
            "#[rudzio::suite([(runtime = ::rudzio::runtime::tokio::Multithread::new, \
             suite = ::rudzio::common::context::Suite, \
             test = ::rudzio::common::context::Test)])]\n\
             mod inner { #[rudzio::test] fn bar_test() {} }\n",
        )?;
        let combined = emit_combined(temp.path())?;
        anyhow::ensure!(
            combined.matches("foo.rs").count() == 1,
            "`foo.rs` must be mounted once; got {} mentions:\n{combined}",
            combined.matches("foo.rs").count()
        );
        anyhow::ensure!(
            combined.matches("bar.rs").count() == 1,
            "`bar.rs` must be mounted once; got {} mentions:\n{combined}",
            combined.matches("bar.rs").count()
        );
        Ok(())
    }

    /// Helper file (`common.rs`) declared via `mod common;` from
    /// the test root. The helper must be mounted exactly once: at
    /// the aggregator's crate root (Bug #8 fix). The proxy mod tree
    /// has its `mod common;` decl stripped (Bug #10 fix) so the
    /// helper doesn't compile twice. Net mention count of
    /// `common.rs` across main.rs + tests.rs: exactly 1.
    #[rudzio::test]
    fn helper_file_mounted_once_across_main_and_tests() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        let test_root_dir = write_single_binary_workspace(temp.path(), &rudzio_root)?;
        fs::write(
            test_root_dir.join("mod.rs"),
            "// rudzio_test marker\n\
             mod common;\n\
             mod foo;\n\
             #[rudzio::main]\nfn main() {}\n",
        )?;
        fs::write(
            test_root_dir.join("common.rs"),
            "pub const TOKEN: () = ();\n",
        )?;
        fs::write(
            test_root_dir.join("foo.rs"),
            "use crate::common::TOKEN;\n\
             #[rudzio::suite([(runtime = ::rudzio::runtime::tokio::Multithread::new, \
             suite = ::rudzio::common::context::Suite, \
             test = ::rudzio::common::context::Test)])]\n\
             mod inner { #[rudzio::test] fn t() { let _: () = TOKEN; } }\n",
        )?;
        let combined = emit_combined(temp.path())?;
        let common_mounts = combined.matches("common.rs").count();
        anyhow::ensure!(
            common_mounts == 1,
            "`common.rs` must be mounted once (crate-root only; proxy mod-decl stripped); \
             got {common_mounts} mentions:\n{combined}",
        );
        Ok(())
    }

    /// Two binaries each with a `foo.rs` test file. Per-binary
    /// scoping must keep them independent — total mentions across
    /// main.rs + tests.rs should be 2 (one per binary), NOT 1
    /// (which would mean cross-binary dedup collapsed them).
    #[rudzio::test]
    fn cross_binary_identical_stem_files_each_mount_once() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        let (a_dir, b_dir) = write_two_binary_workspace(temp.path(), &rudzio_root)?;
        for dir in [&a_dir, &b_dir] {
            fs::write(
                dir.join("mod.rs"),
                "// rudzio_test marker\n\
                 mod foo;\n\
                 #[rudzio::main]\nfn main() {}\n",
            )?;
            fs::write(
                dir.join("foo.rs"),
                "#[rudzio::suite([(runtime = ::rudzio::runtime::tokio::Multithread::new, \
                 suite = ::rudzio::common::context::Suite, \
                 test = ::rudzio::common::context::Test)])]\n\
                 mod inner { #[rudzio::test] fn t() {} }\n",
            )?;
        }
        let combined = emit_combined(temp.path())?;
        // Each binary's `foo.rs` should appear exactly once. Total
        // mentions: 2 (one per binary). NOT 1 (collapsed) and NOT
        // 4 (double-mounted).
        let foo_mounts = combined.matches("foo.rs").count();
        anyhow::ensure!(
            foo_mounts == 2,
            "two binaries' `foo.rs` must each mount once (total 2); \
             got {foo_mounts} mentions:\n{combined}",
        );
        Ok(())
    }
}
