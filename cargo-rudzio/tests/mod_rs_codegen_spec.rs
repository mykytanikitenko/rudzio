//! Behavioral spec: aggregator codegen handles `tests/<name>/mod.rs`
//! entry points without emitting `mod mod;` (a Rust keyword collision).
//!
//! # Symptom (pre-fix)
//!
//! A consumer member that declares `[[test]] path = "tests/foo/mod.rs"`
//! caused `build_tests_rs` to emit `mod mod;` in
//! `target/rudzio-auto-runner/src/tests.rs`. Compilation halted with
//! `expected identifier, found keyword "mod"`. Only workaround was to
//! rename the entry point away from `mod.rs` or hand-patch the
//! generated file after every `cargo rudzio test` invocation.
//!
//! # Root cause
//!
//! `build_tests_rs` builds each `#[path = "..."] mod <ident>;` line
//! from `file.file_stem()`. For `tests/foo/mod.rs` the stem is
//! literally `mod`, which `sanitize_ident` (only escaping non-
//! alphanumerics) returns unchanged. The emitter has no Rust-keyword
//! guard.
//!
//! # Behavioral assertion
//!
//! Build a single-member synthetic workspace with `[[test]] path =
//! "tests/integration_rudzio/mod.rs"`, drive `plan_from_manifest` +
//! `write_runner`, then read the generated `src/tests.rs` and assert
//! it does NOT contain `mod mod;` and DOES contain a valid
//! `mod integration_rudzio;` (the parent-dir name is the idiomatic
//! Rust module name for a `mod.rs` entry point).

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use rudzio::common::context::Suite;
use rudzio::runtime::async_std::Runtime as AsyncStdRuntime;
use rudzio::runtime::compio::Runtime as CompioRuntime;
use rudzio::runtime::embassy::Runtime as EmbassyRuntime;
use rudzio::runtime::futures::ThreadPool as FuturesThreadPool;
use rudzio::runtime::smol::Runtime as SmolRuntime;
use rudzio::runtime::tokio::{
    CurrentThread as TokioCurrentThread, Local as TokioLocal, Multithread as TokioMultithread,
};
use tempfile::TempDir;

/// Workspace root of the rudzio repo (so synthetic fixtures can
/// `path = <this>` rudzio and exercise the in-tree code).
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Materialise a single-member workspace at `root` where the consumer
/// declares its sole integration-test entry at
/// `tests/integration_rudzio/mod.rs`. The bridge / aggregator must
/// `#[path]`-include this file under a valid Rust identifier; pre-fix
/// the emitter picks `mod` (file stem), producing invalid source.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
            rudzio = rudzio_root.display(),
        ),
    )?;

    fs::create_dir_all(root.join("consumer").join("src"))?;
    fs::create_dir_all(
        root.join("consumer")
            .join("tests")
            .join("integration_rudzio"),
    )?;
    fs::write(
        root.join("consumer").join("Cargo.toml"),
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
         name = \"integration_rudzio\"\n\
         path = \"tests/integration_rudzio/mod.rs\"\n\
         harness = false\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = \"=1.0.102\"\n",
    )?;
    fs::write(
        root.join("consumer").join("src").join("lib.rs"),
        "// rudzio_test marker\n",
    )?;
    // `tests/integration_rudzio/mod.rs` — the entry point with the
    // problematic file stem (`mod`). Body is empty: this test only
    // exercises the file-name → module-identifier translation, not
    // the test discovery / dispatch downstream.
    fs::write(
        root.join("consumer")
            .join("tests")
            .join("integration_rudzio")
            .join("mod.rs"),
        "// rudzio_test marker\n",
    )?;

    Ok(())
}

/// Set `CARGO_NET_OFFLINE=true` for the duration of `body`, restoring
/// the previous value on exit. Prevents `cargo metadata` from trying
/// to refresh registry indexes for the synthetic fixture.
fn with_cargo_offline<F, T>(body: F) -> T
where
    F: FnOnce() -> T,
{
    /// RAII guard that restores `CARGO_NET_OFFLINE` to its previous
    /// value when dropped. Survives panics in `body`.
    struct Restore {
        /// Value observed before the guard installed; `None` = was unset.
        prev: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                #[expect(
                    unsafe_code,
                    reason = "std::env::set_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see module-level comment on with_cargo_offline.
                unsafe {
                    env::set_var("CARGO_NET_OFFLINE", prev);
                }
            } else {
                #[expect(
                    unsafe_code,
                    reason = "std::env::remove_var marked unsafe; this test owns the env var for the duration of the guard, no concurrent setters"
                )]
                // SAFETY: see module-level comment on with_cargo_offline.
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
    (runtime = TokioCurrentThread::new, suite = Suite, test = Test),
    (runtime = TokioLocal::new, suite = Suite, test = Test),
    (runtime = CompioRuntime::new, suite = Suite, test = Test),
    (runtime = EmbassyRuntime::new, suite = Suite, test = Test),
    (runtime = FuturesThreadPool::new, suite = Suite, test = Test),
    (runtime = AsyncStdRuntime::new, suite = Suite, test = Test),
    (runtime = SmolRuntime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        PathBuf, TempDir, fs, plan_from_manifest, rudzio_source_root, with_cargo_offline,
        write_fixture, write_runner,
    };

    /// Drive the aggregator pipeline against the `mod.rs` fixture and
    /// return the path to the generated `src/tests.rs`.
    fn drive_pipeline() -> anyhow::Result<(TempDir, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        Ok((temp, out_dir.join("src").join("tests.rs")))
    }

    #[rudzio::test]
    fn generated_tests_rs_does_not_emit_mod_mod_keyword_collision() -> anyhow::Result<()> {
        let (_temp, tests_rs) = drive_pipeline()?;
        let body = fs::read_to_string(&tests_rs)
            .map_err(|err| anyhow::anyhow!("reading {}: {err}", tests_rs.display()))?;
        // The bug emits `mod mod;` (whitespace-normalised). Compare on
        // the exact tokens so a future emitter style change (e.g.
        // `pub mod mod;`) can't slip past the assertion.
        anyhow::ensure!(
            !body.contains("mod mod;") && !body.contains("mod mod {"),
            "generated tests.rs contains `mod mod` keyword collision:\n{body}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn generated_tests_rs_picks_parent_dir_name_for_mod_rs_entry() -> anyhow::Result<()> {
        let (_temp, tests_rs) = drive_pipeline()?;
        let body = fs::read_to_string(&tests_rs)
            .map_err(|err| anyhow::anyhow!("reading {}: {err}", tests_rs.display()))?;
        // For `tests/integration_rudzio/mod.rs` the idiomatic Rust
        // module identifier is the parent directory name. Match the
        // exact `mod integration_rudzio;` shape — substring search
        // would also accept `mod integration_rudzio_helper;` which
        // isn't what we want.
        anyhow::ensure!(
            body.contains("mod integration_rudzio;"),
            "generated tests.rs should emit `mod integration_rudzio;` (parent-dir name) for a mod.rs entry, got:\n{body}",
        );
        Ok(())
    }

    /// Reserved Rust keyword as the test-binary's parent directory
    /// name. Cargo accepts `path = "tests/impl/mod.rs"` (just a file
    /// path), but Rust ident rules forbid `mod impl;` — the codegen
    /// must emit the raw-identifier form `mod r#impl;`.
    #[rudzio::test]
    fn generated_tests_rs_escapes_reserved_keyword_parent_dir() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        // Custom fixture: dir named `impl` (a Rust keyword).
        fs::write(
            temp.path().join("Cargo.toml"),
            format!(
                "[workspace]\n\
                 members = [\"consumer\"]\n\
                 resolver = \"3\"\n\
                 \n\
                 [workspace.dependencies]\n\
                 rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
                rudzio = rudzio_root.display(),
            ),
        )?;
        let consumer = temp.path().join("consumer");
        fs::create_dir_all(consumer.join("src"))?;
        fs::create_dir_all(consumer.join("tests").join("impl"))?;
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
             name = \"impl_tests\"\n\
             path = \"tests/impl/mod.rs\"\n\
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
            consumer.join("tests").join("impl").join("mod.rs"),
            "// rudzio_test marker\n",
        )?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        let body = fs::read_to_string(out_dir.join("src").join("tests.rs"))?;
        // Must escape as `mod r#impl;` — the bare `mod impl;` form
        // would fail to compile.
        anyhow::ensure!(
            body.contains("mod r#impl;"),
            "reserved keyword `impl` as parent dir must be escaped via `mod r#impl;`:\n{body}",
        );
        anyhow::ensure!(
            !body.contains("\nmod impl;") && !body.contains(" mod impl;"),
            "bare `mod impl;` is invalid Rust; codegen must use r# prefix:\n{body}",
        );
        Ok(())
    }

    /// Deep nesting: `tests/<a>/<b>/<c>/mod.rs`. The codegen picks
    /// the IMMEDIATE parent dir name (`c`), not a flattened
    /// path-derived ident. Guards against a regression that would
    /// emit `mod a_b_c;` or similar.
    #[rudzio::test]
    fn generated_tests_rs_uses_immediate_parent_for_deeply_nested_mod_rs() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        fs::write(
            temp.path().join("Cargo.toml"),
            format!(
                "[workspace]\n\
                 members = [\"consumer\"]\n\
                 resolver = \"3\"\n\
                 \n\
                 [workspace.dependencies]\n\
                 rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
                rudzio = rudzio_root.display(),
            ),
        )?;
        let consumer = temp.path().join("consumer");
        let deep = consumer.join("tests").join("a").join("b").join("c");
        fs::create_dir_all(consumer.join("src"))?;
        fs::create_dir_all(&deep)?;
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
             name = \"deeply_nested\"\n\
             path = \"tests/a/b/c/mod.rs\"\n\
             harness = false\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true }\n",
        )?;
        fs::write(
            consumer.join("src").join("lib.rs"),
            "// rudzio_test marker\n",
        )?;
        fs::write(deep.join("mod.rs"), "// rudzio_test marker\n")?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        write_runner(&plan, &out_dir)?;
        let body = fs::read_to_string(out_dir.join("src").join("tests.rs"))?;
        anyhow::ensure!(
            body.contains("mod c;"),
            "deep `a/b/c/mod.rs` must use immediate-parent ident `c`:\n{body}",
        );
        anyhow::ensure!(
            !body.contains("mod a_b_c;"),
            "codegen must NOT flatten the path into a joined ident:\n{body}",
        );
        Ok(())
    }

    /// Hyphenated parent dir name. Cargo accepts hyphens in path
    /// targets (`tests/my-test/mod.rs`); Rust idents do NOT permit
    /// hyphens, so the codegen MUST either:
    ///
    /// 1. Sanitize the hyphen to underscore (`mod my_test;`), or
    /// 2. Bail with a clear error.
    ///
    /// Silently emitting `mod my-test;` would produce invalid Rust
    /// source. The test accepts either valid outcome but rejects the
    /// silent-invalid form.
    #[rudzio::test]
    fn generated_tests_rs_handles_hyphenated_parent_dir() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        fs::write(
            temp.path().join("Cargo.toml"),
            format!(
                "[workspace]\n\
                 members = [\"consumer\"]\n\
                 resolver = \"3\"\n\
                 \n\
                 [workspace.dependencies]\n\
                 rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
                rudzio = rudzio_root.display(),
            ),
        )?;
        let consumer = temp.path().join("consumer");
        fs::create_dir_all(consumer.join("src"))?;
        fs::create_dir_all(consumer.join("tests").join("my-test"))?;
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
             name = \"my_test\"\n\
             path = \"tests/my-test/mod.rs\"\n\
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
            consumer.join("tests").join("my-test").join("mod.rs"),
            "// rudzio_test marker\n",
        )?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = temp.path().join("rudzio-auto-runner");
        // The pipeline must either bail with a clear error (option 2)
        // OR succeed and emit a valid identifier (option 1). Silent
        // emission of invalid Rust is the only outcome the test
        // refuses.
        match write_runner(&plan, &out_dir) {
            Ok(()) => {
                let body = fs::read_to_string(out_dir.join("src").join("tests.rs"))?;
                anyhow::ensure!(
                    !body.contains("mod my-test;"),
                    "codegen emitted invalid `mod my-test;` (hyphen in Rust ident); \
                     must sanitize or bail:\n{body}",
                );
                anyhow::ensure!(
                    body.contains("mod my_test;") || body.contains("mod r#my_test;"),
                    "codegen accepted hyphenated dir but didn't emit a sanitized ident; \
                     expected `mod my_test;`:\n{body}",
                );
            }
            Err(err) => {
                let chain = format!("{err:?}");
                anyhow::ensure!(
                    chain.contains("my-test")
                        || chain.contains("hyphen")
                        || chain.contains("ident"),
                    "if codegen bails on hyphenated dir, the diagnostic must name the cause; \
                     got:\n{chain}",
                );
            }
        }
        Ok(())
    }
}
