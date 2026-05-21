//! Behavioral spec: every `#[rudzio::test]` is registered into the
//! aggregator's `TEST_TOKENS` distributed slice EXACTLY ONCE
//! (Bug #11).
//!
//! # Symptom (pre-fix)
//!
//! cargo-rudzio 0.1.1 emits each `mod <helper>;` declaration twice:
//! once nested under the test-root's `mod.rs` (which keeps every
//! `mod <name>;` it found), and once at the aggregator's crate root
//! (via the Bug #8 fix that hoists helpers there with bare names).
//! When the helper is itself a test FILE (`mod foo;` where `foo.rs`
//! has `#[rudzio::test]` functions), the file compiles TWICE under
//! two distinct module paths:
//!
//!   * `__rudzio_member_tests::<member>::integration::foo::*`
//!     (the natural nested aggregator path).
//!   * `foo::*` (or `<synth>::*` under the new Bug #10 work) —
//!     hoisted at crate root.
//!
//! Each `#[rudzio::test]` macro invocation registers ONE
//! `linkme::distributed_slice` entry. Two compile units = two
//! entries = the same test body executes twice per `cargo rudzio
//! test` run. Doubles wall time, doubles DB writes / container
//! traffic, breaks test-count reporting.
//!
//! # Behavioral assertion
//!
//! Synthesize a member with a single `[[test]] integration` binary
//! at `tests/integration/mod.rs`, holding one `#[rudzio::test] fn`
//! inside `tests/integration/foo.rs`. Drive `plan_from_manifest` +
//! `write_runner`. Inspect the generated `src/main.rs` AND
//! `src/tests.rs` for `#[path = "<…>/foo.rs"]` references and
//! assert the file is mounted EXACTLY ONCE. Catching this at the
//! codegen layer is faster than actually running the aggregator
//! and equally diagnostic — every double-mount means a double
//! `#[rudzio::test]` registration.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the rudzio repo. Synthetic fixtures `path =`
/// the in-tree rudzio so `cargo metadata` resolves without network.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Minimal fixture: one member, one `[[test]]` binary, one test
/// file. mod.rs declares `mod foo;`. foo.rs holds the
/// `#[rudzio::test]`. The simplest layout the bug needs.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"member\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", default-features = false, features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;
    let member = root.join("member");
    fs::create_dir_all(member.join("src"))?;
    fs::create_dir_all(member.join("tests").join("integration"))?;
    fs::write(
        member.join("Cargo.toml"),
        "[package]\n\
         name = \"member\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"member\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [[test]]\n\
         name = \"integration\"\n\
         path = \"tests/integration/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.102\"\n",
    )?;
    fs::write(member.join("src").join("lib.rs"), "// rudzio_test marker\n")?;
    fs::write(
        member.join("tests").join("integration").join("mod.rs"),
        "// rudzio_test marker\n\
         #![allow(clippy::tests_outside_test_module)]\n\
         \n\
         mod foo;\n\
         \n\
         #[rudzio::main]\n\
         fn main() {}\n",
    )?;
    fs::write(
        member.join("tests").join("integration").join("foo.rs"),
        "use rudzio::common::context::{Suite, Test};\n\
         use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         mod tests {\n\
         \x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20fn just_one_test() -> anyhow::Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )?;
    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard restoring `CARGO_NET_OFFLINE` on drop.
    struct Restore {
        /// Value observed before installation; `None` = was unset.
        prev: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                #[expect(
                    unsafe_code,
                    reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see other specs in this directory.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see other specs in this directory.
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
        reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
    )]
    // SAFETY: see Restore::drop.
    unsafe {
        env::set_var("CARGO_NET_OFFLINE", "true");
    }
    body()
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        fs, plan_from_manifest, rudzio_source_root, with_cargo_offline, write_fixture, write_runner,
    };

    #[rudzio::test]
    fn each_test_file_is_mounted_exactly_once_in_aggregator() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;

        // The test source file `tests/integration/foo.rs` (or its
        // proxy under `target/.../src/proxies/...`) must be
        // `#[path]`-mounted EXACTLY ONCE across both `src/main.rs`
        // and `src/tests.rs`. Two mounts means foo.rs compiles twice
        // and every `#[rudzio::test]` inside registers TWO entries
        // into `linkme::distributed_slice TEST_TOKENS` — Bug #11.
        let main_rs = fs::read_to_string(out_dir.join("src").join("main.rs"))?;
        let tests_rs = fs::read_to_string(out_dir.join("src").join("tests.rs"))?;
        let combined = format!("{main_rs}\n{tests_rs}");
        let foo_mounts = combined.matches("foo.rs").count();
        anyhow::ensure!(
            foo_mounts == 1,
            "foo.rs must be `#[path]`-mounted exactly once across the aggregator's main.rs + tests.rs; found {foo_mounts} mentions\n--- main.rs ---\n{main_rs}\n--- tests.rs ---\n{tests_rs}",
        );
        Ok(())
    }
}
