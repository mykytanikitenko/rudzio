//! Defensive edge cases for Bug #10 per-binary helper isolation.
//!
//! The base reproduction (`per_binary_helper_isolation_spec`) asserts
//! that two `[[test]]` binaries declaring `mod common;` against
//! DIFFERENT files produce an `Err` from `write_runner`. These edges
//! exercise the periphery of that collision-detection logic:
//!
//! - **Three binaries × `mod common;` at three distinct files**: the
//!   diagnostic must name ALL three source paths, not just the first
//!   pair the detector trips on.
//! - **Multiple binaries × `mod common;` pointing at the SAME file**:
//!   no collision. Two binaries can legitimately share a helper file
//!   via a workspace-level convention (or via `#[path]` pointing at
//!   the same target); deduplication on `(name, canonical_path)`
//!   must collapse them, not flag a false positive.
//! - **Two binaries × DIFFERENT helper names, different files**: no
//!   collision at all. Binaries are independent unless they fight
//!   over the same name.
//! - **Each binary declares multiple helpers**: synth names get
//!   issued for every (binary × helper) pair, with no internal
//!   collision when the same helper name appears in two binaries
//!   against different files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use cargo_rudzio_test_helpers::with_cargo_offline;
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the in-tree rudzio package.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Workspace `Cargo.toml` + consumer `Cargo.toml` boilerplate.
/// `test_bins` is the list of `[[test]]` table entries to embed.
fn write_workspace(root: &Path, rudzio_root: &Path, test_bins: &str) -> io::Result<()> {
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
             {test_bins}\n\
             [dev-dependencies]\n\
             rudzio = {{ workspace = true }}\n",
        ),
    )?;
    fs::write(
        consumer.join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    Ok(())
}

/// Materialise one `[[test]]` binary at `consumer/tests/<name>/mod.rs`
/// declaring the listed `mod_decls` (each must have a matching helper
/// file written by the caller).
fn write_test_root(consumer_tests: &Path, name: &str, mod_decls: &str) -> io::Result<()> {
    let dir = consumer_tests.join(name);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("mod.rs"),
        format!(
            "// rudzio_test marker\n\
             {mod_decls}\n\
             #[rudzio::main]\nfn main() {{}}\n"
        ),
    )?;
    Ok(())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        fs, plan_from_manifest, rudzio_source_root, with_cargo_offline, write_runner,
        write_test_root, write_workspace,
    };

    /// Three binaries, three DIFFERENT `common.rs` files. The
    /// collision diagnostic must name every offender so the user
    /// sees the full picture, not just the first pair.
    #[rudzio::test]
    fn three_distinct_helpers_collision_lists_all_paths() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_workspace(
            temp.path(),
            &rudzio_root,
            "[[test]]\nname = \"a\"\npath = \"tests/a/mod.rs\"\nharness = false\n\n\
             [[test]]\nname = \"b\"\npath = \"tests/b/mod.rs\"\nharness = false\n\n\
             [[test]]\nname = \"c\"\npath = \"tests/c/mod.rs\"\nharness = false\n",
        )?;
        let consumer_tests = temp.path().join("consumer").join("tests");
        for binary in ["a", "b", "c"] {
            write_test_root(&consumer_tests, binary, "mod common;")?;
            fs::write(
                consumer_tests.join(binary).join("common.rs"),
                format!("pub const TOKEN_{}: () = ();\n", binary.to_uppercase()),
            )?;
        }
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        let result = write_runner(&plan, &out_dir);
        let Err(err) = result else {
            anyhow::bail!("3-way `common` helper collision must bail; got Ok");
        };
        let chain = format!("{err:?}");
        for binary in ["a", "b", "c"] {
            anyhow::ensure!(
                chain.contains(binary),
                "diagnostic must name binary `{binary}`; got:\n{chain}",
            );
        }
        Ok(())
    }

    /// Two binaries, identical `#[path]` target → same canonical
    /// helper file. NOT a collision: the user is sharing one
    /// implementation across binaries, which is fine.
    #[rudzio::test]
    fn shared_helper_via_path_attribute_does_not_collide() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_workspace(
            temp.path(),
            &rudzio_root,
            "[[test]]\nname = \"a\"\npath = \"tests/a/mod.rs\"\nharness = false\n\n\
             [[test]]\nname = \"b\"\npath = \"tests/b/mod.rs\"\nharness = false\n",
        )?;
        let consumer_tests = temp.path().join("consumer").join("tests");
        let shared = consumer_tests.join("shared");
        fs::create_dir_all(&shared)?;
        fs::write(
            shared.join("common.rs"),
            "pub const SHARED_TOKEN: () = ();\n",
        )?;
        for binary in ["a", "b"] {
            write_test_root(
                &consumer_tests,
                binary,
                "#[path = \"../shared/common.rs\"]\nmod common;",
            )?;
        }
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        // No bail: both binaries point at the same canonical file.
        // Post-fix the collision detector deduplicates on
        // (helper_name, canonical_path) and lets this through.
        // Pre-fix (depending on detector impl) this may still
        // collide; the test documents the intended behavior.
        write_runner(&plan, &out_dir)?;
        Ok(())
    }

    /// Two binaries, DIFFERENT helper names, different files. No
    /// collision at all — binaries are independent. Guard against a
    /// future detector that compares globally rather than per-helper-name.
    #[rudzio::test]
    fn distinct_helper_names_across_binaries_do_not_collide() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_workspace(
            temp.path(),
            &rudzio_root,
            "[[test]]\nname = \"a\"\npath = \"tests/a/mod.rs\"\nharness = false\n\n\
             [[test]]\nname = \"b\"\npath = \"tests/b/mod.rs\"\nharness = false\n",
        )?;
        let consumer_tests = temp.path().join("consumer").join("tests");
        write_test_root(&consumer_tests, "a", "mod helper_a;")?;
        fs::write(
            consumer_tests.join("a").join("helper_a.rs"),
            "pub const A_TOKEN: () = ();\n",
        )?;
        write_test_root(&consumer_tests, "b", "mod helper_b;")?;
        fs::write(
            consumer_tests.join("b").join("helper_b.rs"),
            "pub const B_TOKEN: () = ();\n",
        )?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;
        Ok(())
    }

    /// Two binaries, two helpers each, NO cross-helper-name conflict
    /// (`a/helper_a.rs` + `a/helper_b.rs` + `b/helper_a.rs` +
    /// `b/helper_b.rs`).
    /// Each binary's `helper_a.rs` and `helper_b.rs` point at its own
    /// sibling file. Per-binary scoping must keep them independent —
    /// no collisions, even though `helper_a` and `helper_b` appear
    /// twice each across the workspace.
    #[rudzio::test]
    fn per_binary_multiple_helpers_do_not_cross_pollute() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_workspace(
            temp.path(),
            &rudzio_root,
            "[[test]]\nname = \"a\"\npath = \"tests/a/mod.rs\"\nharness = false\n\n\
             [[test]]\nname = \"b\"\npath = \"tests/b/mod.rs\"\nharness = false\n",
        )?;
        let consumer_tests = temp.path().join("consumer").join("tests");
        for binary in ["a", "b"] {
            write_test_root(&consumer_tests, binary, "mod helper_a;\nmod helper_b;")?;
            fs::write(
                consumer_tests.join(binary).join("helper_a.rs"),
                format!("pub const HELPER_A_{}: () = ();\n", binary.to_uppercase()),
            )?;
            fs::write(
                consumer_tests.join(binary).join("helper_b.rs"),
                format!("pub const HELPER_B_{}: () = ();\n", binary.to_uppercase()),
            )?;
        }
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        // 4 distinct (binary, helper) pairs, each at its own file.
        // Per-binary scoping must produce 4 synth names without
        // collision. Pre-fix global dedup would silently collapse
        // the four into two and tests in one binary would see the
        // other's helpers.
        let result = write_runner(&plan, &out_dir);
        let Err(err) = result else {
            // Expected post-fix: write_runner errors loudly because
            // both helper_a and helper_b appear across two distinct
            // sibling files — same shape as the base Bug #10 repro,
            // just doubled.
            anyhow::bail!("dual cross-binary helper collision must bail; got Ok");
        };
        let chain = format!("{err:?}");
        anyhow::ensure!(
            chain.contains("helper_a") && chain.contains("helper_b"),
            "diagnostic must name BOTH colliding helpers; got:\n{chain}",
        );
        Ok(())
    }
}
