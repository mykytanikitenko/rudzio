//! E2E coverage for cargo-rudzio's forwarding of a workspace member's
//! `build.rs` into the generated bridge crate.
//!
//! Each case materialises a synthetic workspace member with a `build.rs`
//! (or an explicit `package.build = "..."` script) and drives
//! `cargo-rudzio test` (no `-p`), asserting the member's build-script
//! effects — `OUT_DIR` generated files, `cargo:rustc-cfg`, `cargo:rustc-env`,
//! and `CARGO_MANIFEST_DIR`-relative file reads — are visible to the
//! member's `src` tests under the bridge compile.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// `a`'s build script (case 1): writes `$OUT_DIR/gen.rs`.
const A_BUILD_RS: &str = "use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var(\"OUT_DIR\").expect(\"OUT_DIR\"));
    fs::write(out_dir.join(\"gen.rs\"), \"pub const GEN: u32 = 7;\\n\").expect(\"write gen.rs\");
}
";

/// `a`'s lib (case 1): `include!`s the `$OUT_DIR/gen.rs` the build script wrote.
const A_LIB_RS: &str = "// rudzio_test marker
include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn gen_value(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(crate::GEN == 7, \"GEN should be 7, got {}\", crate::GEN);
            Ok(())
        }
    }
}
";

/// `b`'s build script (case 2): emits `rustc-cfg` + `rustc-check-cfg`.
const B_BUILD_RS: &str = "fn main() {
    println!(\"cargo:rustc-cfg=has_feature_x\");
    println!(\"cargo::rustc-check-cfg=cfg(has_feature_x)\");
}
";

/// `b`'s lib (case 2): carries a `#[cfg(has_feature_x)]` fn the test asserts.
const B_LIB_RS: &str = "// rudzio_test marker
#[cfg(has_feature_x)]
pub fn feature_x_value() -> u32 {
    42
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn feature_x(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(crate::feature_x_value() == 42);
            Ok(())
        }
    }
}
";

/// `c`'s build script (case 3): reads `data/input.txt` relative to
/// `CARGO_MANIFEST_DIR` and emits its content as a `rustc-env`.
const C_BUILD_RS: &str = "use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var(\"CARGO_MANIFEST_DIR\").expect(\"CARGO_MANIFEST_DIR\"));
    let content = fs::read_to_string(manifest_dir.join(\"data\").join(\"input.txt\")).expect(\"read input.txt\");
    println!(\"cargo:rustc-env=DATA_CONTENT={}\", content.trim());
}
";

/// `c`'s lib (case 3): asserts `DATA_CONTENT` carries `data/input.txt`.
const C_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn data_content(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                option_env!(\"DATA_CONTENT\") == Some(\"hello-from-data\"),
                \"DATA_CONTENT should be set from data/input.txt\"
            );
            Ok(())
        }
    }
}
";

/// `d`'s build script (case 4): explicit non-default `package.build` path.
const D_BUILD_RS: &str = "fn main() {
    println!(\"cargo:rustc-env=CUSTOM_BUILD=yes\");
}
";

/// `d`'s lib (case 4): asserts `CUSTOM_BUILD` from the explicit build path.
const D_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn custom_build(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                option_env!(\"CUSTOM_BUILD\") == Some(\"yes\"),
                \"explicit package.build script did not run\"
            );
            Ok(())
        }
    }
}
";

/// `e`'s build script (case 5): emits `E_BUILD`.
const E_BUILD_RS: &str = "fn main() {
    println!(\"cargo:rustc-env=E_BUILD=yes\");
}
";

/// `e`'s lib (case 5): src test asserting `E_BUILD`.
const E_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn e_src_test(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(option_env!(\"E_BUILD\") == Some(\"yes\"), \"E_BUILD unset in src test\");
            Ok(())
        }
    }
}
";

/// A rudzio integration-test source (case 5) asserting `E_BUILD`.
const E_INTEGRATION_RS: &str = "// rudzio_test marker
#![allow(clippy::tests_outside_test_module)]

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
mod tests {
    #[rudzio::test]
    fn e_integration_test() -> anyhow::Result<()> {
        anyhow::ensure!(option_env!(\"E_BUILD\") == Some(\"yes\"), \"E_BUILD unset in integration test\");
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
";

/// `f`'s lib (case 6, control): no build script at all.
const F_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn control_test(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(2 + 2 == 4);
            Ok(())
        }
    }
}
";

/// Isolated cargo locations the fixture run is pinned to.
struct CargoEnv<'paths> {
    /// `CARGO_HOME` for the subprocess.
    home: &'paths Path,
    /// `CARGO_TARGET_DIR` for the subprocess.
    target_dir: &'paths Path,
    /// Parent directory for the fixture workspace.
    tempdir: &'paths Path,
}

/// Repo root = parent of `cargo-rudzio/`. The fixture's `rudzio`
/// workspace dependency points here so it builds against the code under
/// test.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Materialise a workspace-root `Cargo.toml` with the given `members`
/// array and the shared `[workspace.dependencies]`.
fn write_workspace(root: &Path, members: &str) -> io::Result<()> {
    let rudzio = rudzio_source_root()
        .ok_or_else(|| io::Error::other("cargo-rudzio/tests must live under the workspace root"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nmembers = {members}\nresolver = \"3\"\n\n[workspace.dependencies]\nrudzio = {{ path = \"{rudzio}\", features = [\"runtime-tokio-multi-thread\", \"common\"] }}\nanyhow = \"=1.0.104\"\n",
            rudzio = rudzio.display(),
        ),
    )
}

/// Render a member manifest. `sections` sits between `[lib]` and
/// `[dev-dependencies]` (holds `[build-dependencies]`, features, etc.).
fn manifest(name: &str, sections: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\n\n{sections}[dev-dependencies]\nrudzio = {{ workspace = true }}\nanyhow = {{ workspace = true }}\n"
    )
}

/// Render a member manifest with an explicit `build` script in
/// `[package]` (a non-default path that cargo cannot auto-detect).
fn manifest_with_build(name: &str, build: &str, sections: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\nbuild = \"{build}\"\n\n[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\n\n{sections}[dev-dependencies]\nrudzio = {{ workspace = true }}\nanyhow = {{ workspace = true }}\n"
    )
}

/// Write a member crate at repo-relative path `rel`.
fn write_member(root: &Path, rel: &str, manifest: &str, lib_rs: &str) -> io::Result<()> {
    let dir = root.join(rel);
    fs::create_dir_all(dir.join("src"))?;
    fs::write(dir.join("Cargo.toml"), manifest)?;
    fs::write(dir.join("src").join("lib.rs"), lib_rs)?;
    Ok(())
}

/// Write an arbitrary file at repo-relative path `rel` (creating parent dirs).
fn write_file(root: &Path, rel: &str, filename: &str, content: &str) -> io::Result<()> {
    let dir = root.join(rel);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(filename), content)?;
    Ok(())
}

/// Spawn `cargo-rudzio test` (no `-p` unless `extra` supplies it) from
/// `root` against the isolated cargo env.
fn run(cargo: &CargoEnv<'_>, root: &Path, extra: &[&str]) -> anyhow::Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
        .arg("test")
        .args(extra)
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo.home)
        .env("CARGO_TARGET_DIR", cargo.target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .output()?)
}

/// Parse the leaf names (after the last `::`) of every `[OK] <name> ...`
/// line, excluding suite `setup` / `teardown` hooks.
fn passed_leaf_names(stdout: &str) -> BTreeSet<String> {
    let mut leaves = BTreeSet::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("[OK] ") else {
            continue;
        };
        let name = rest.split_whitespace().next().unwrap_or("");
        let leaf = name.rsplit("::").next().unwrap_or(name);
        if leaf == "setup" || leaf == "teardown" {
            continue;
        }
        let _inserted: bool = leaves.insert(leaf.to_owned());
    }
    leaves
}

/// Parse `N` from the `test result: ok. N passed; ...` summary line.
fn passed_count(stdout: &str) -> Option<usize> {
    for line in stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix("test result: ok.") else {
            continue;
        };
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
    }
    None
}

/// Assert the run exited 0, the passed leaf set equals `expected`
/// exactly, and the summary `N passed` equals `expected.len()`.
fn assert_exact_passes(output: &Output, expected: &[&str]) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    anyhow::ensure!(
        output.status.success(),
        "run exited {:?}\n{context}",
        output.status.code(),
    );
    let leaves = passed_leaf_names(&stdout);
    let expected_set: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();
    anyhow::ensure!(
        leaves == expected_set,
        "passed leaf set mismatch\n  got:      {leaves:?}\n  expected: {expected_set:?}\n{context}"
    );
    let count = passed_count(&stdout).ok_or_else(|| {
        anyhow::anyhow!("no `test result: ok. N passed` summary found\n{context}")
    })?;
    anyhow::ensure!(
        count == expected.len(),
        "pass count {count} != expected {} ({expected_set:?})\n{context}",
        expected.len(),
    );
    Ok(())
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use super::{
        A_BUILD_RS, A_LIB_RS, B_BUILD_RS, B_LIB_RS, C_BUILD_RS, C_LIB_RS, CargoEnv, D_BUILD_RS,
        D_LIB_RS, E_BUILD_RS, E_INTEGRATION_RS, E_LIB_RS, F_LIB_RS, IsolatedCargoTest,
        assert_exact_passes, manifest, manifest_with_build, run, write_file, write_member,
        write_workspace,
    };
    use tempfile::TempDir;

    /// Build a [`CargoEnv`] from a test context. A macro (not a fn)
    /// because `IsolatedCargoTest` carries a runtime generic that the
    /// `#[rudzio::test]` rewrite elides in test-body signatures.
    macro_rules! cargo_env {
        ($ctx:expr) => {
            CargoEnv {
                tempdir: $ctx.suite_tempdir(),
                home: $ctx.cargo_home(),
                target_dir: $ctx.cargo_target_dir(),
            }
        };
    }

    /// Case 1: build.rs writes `$OUT_DIR/gen.rs`; src `include!`s it and
    /// the test asserts the generated `GEN == 7`.
    #[rudzio::test]
    fn out_dir_generated_file_reaches_src(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/a", &manifest("a", ""), A_LIB_RS)?;
        write_file(root, "crates/a", "build.rs", A_BUILD_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["gen_value"])
    }

    /// Case 2: build.rs emits `rustc-cfg=has_feature_x`; src carries a
    /// `#[cfg(has_feature_x)]` fn the test asserts is present.
    #[rudzio::test]
    fn rustc_cfg_reaches_src(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/b", &manifest("b", ""), B_LIB_RS)?;
        write_file(root, "crates/b", "build.rs", B_BUILD_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["feature_x"])
    }

    /// Case 3: build.rs reads `data/input.txt` relative to
    /// `CARGO_MANIFEST_DIR` and emits its content as `rustc-env`.
    #[rudzio::test]
    fn manifest_dir_relative_read_reaches_src(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/c", &manifest("c", ""), C_LIB_RS)?;
        write_file(root, "crates/c", "build.rs", C_BUILD_RS)?;
        write_file(root, "crates/c/data", "input.txt", "hello-from-data\n")?;
        assert_exact_passes(&run(&env, root, &[])?, &["data_content"])
    }

    /// Case 4: explicit non-default `package.build = "tools/gen.rs"`.
    #[rudzio::test]
    fn explicit_non_default_build_path(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/d",
            &manifest_with_build("d", "tools/gen.rs", ""),
            D_LIB_RS,
        )?;
        write_file(root, "crates/d/tools", "gen.rs", D_BUILD_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["custom_build"])
    }

    /// Case 5: member build.rs + src rudzio test + integration test in
    /// `tests/` — both reported.
    #[rudzio::test]
    fn src_and_integration_tests_both_reported(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/e", &manifest("e", ""), E_LIB_RS)?;
        write_file(root, "crates/e", "build.rs", E_BUILD_RS)?;
        write_file(root, "crates/e/tests", "integration.rs", E_INTEGRATION_RS)?;
        assert_exact_passes(
            &run(&env, root, &[])?,
            &["e_src_test", "e_integration_test"],
        )
    }

    /// Case 6 (control): member with no build.rs — unchanged behaviour.
    #[rudzio::test]
    fn no_build_rs_control_unchanged(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/f", &manifest("f", ""), F_LIB_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["control_test"])
    }
}
