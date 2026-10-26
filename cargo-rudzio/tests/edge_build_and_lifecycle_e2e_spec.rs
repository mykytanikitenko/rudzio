//! Proactive EDGE-CASE e2e coverage for (a) member `build.rs`
//! propagation and (b) aggregator lifecycle / cache behaviour.
//!
//! Part A drills the `include!`-splice + `set_var` design that forwards
//! a workspace member's build script into the generated bridge, plus the
//! inner-attribute guard `validate_member_build_rs`. Each case
//! materialises a synthetic member with a `build.rs` (or an explicit
//! `package.build`) and drives `cargo rudzio test`, asserting the
//! member's build-script effects reach its `src` tests.
//!
//! Part B drives the aggregator over repeated and concurrent runs,
//! asserting the cache key is stable across invocations, a consumer
//! source change is picked up on the next run, concurrent invocations
//! with task-local target dirs are isolated, and `generate-runner`
//! produces a buildable, runnable aggregator.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;

// ---------------------------------------------------------------------------
// Part A fixtures
// ---------------------------------------------------------------------------

/// A1: build.rs that begins with an inner attribute (`#![...]`). The
/// `validate_member_build_rs` guard must reject it with an actionable
/// diagnostic naming the member and the hoist-to-outer fix.
const A1_BUILD_RS: &str = "#![allow(dead_code)]

fn main() {}
";

/// A2: build.rs that begins with an inner doc comment (`//!`). Rejected
/// by the same guard with the plain-comment fix.
const A2_BUILD_RS: &str = "//! member build-script documentation
fn main() {}
";

/// A3: build.rs reads `CARGO_MANIFEST_DIR` both at compile time
/// (`env!`, spliced, resolves to the bridge dir) and at runtime
/// (`std::env::var`, repointed to the member dir). The two must diverge.
const A3_BUILD_RS: &str = "fn main() {
    let rt = std::env::var(\"CARGO_MANIFEST_DIR\").expect(\"CARGO_MANIFEST_DIR at runtime\");
    println!(\"cargo:rustc-env=RT_MANIFEST_DIR={rt}\");
    println!(\"cargo:rustc-env=CT_MANIFEST_DIR={}\", env!(\"CARGO_MANIFEST_DIR\"));
}
";

/// `a`'s lib (A3): asserts runtime vs compile-time manifest dirs diverge.
const A3_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn manifest_dir_runtime_vs_compiletime(_t: &super::Test) -> anyhow::Result<()> {
            let rt = option_env!(\"RT_MANIFEST_DIR\").expect(\"RT_MANIFEST_DIR set\");
            let ct = option_env!(\"CT_MANIFEST_DIR\").expect(\"CT_MANIFEST_DIR set\");
            anyhow::ensure!(
                rt != ct,
                \"runtime CARGO_MANIFEST_DIR ({rt}) should differ from compile-time ({ct})\"
            );
            anyhow::ensure!(
                ct.contains(\"rudzio-auto-runner\"),
                \"compile-time env!(CARGO_MANIFEST_DIR) resolves to the bridge dir (contains \
                 'rudzio-auto-runner'), got {ct}\"
            );
            anyhow::ensure!(
                !rt.contains(\"rudzio-auto-runner\"),
                \"runtime CARGO_MANIFEST_DIR is repointed to the member dir (no \
                 'rudzio-auto-runner'), got {rt}\"
            );
            Ok(())
        }
    }
}
";

/// A4: build.rs emits cfg + check-cfg and writes a generated file the
/// src `include!`s. The build script's runtime `OUT_DIR` must equal the
/// src's compile-time `env!("OUT_DIR")` (single-bridge-package
/// unification). Plain cargo rejects `env!("OUT_DIR")` inside a build
/// script, so the build side reads it at runtime.
const A4_BUILD_RS: &str = "fn main() {
    println!(\"cargo:rustc-cfg=has_generated\");
    println!(\"cargo::rustc-check-cfg=cfg(has_generated)\");
    let out_dir = std::env::var(\"OUT_DIR\").expect(\"OUT_DIR\");
    std::fs::write(
        format!(\"{out_dir}/generated.rs\"),
        \"pub const GENERATED: u32 = 99;\\n\",
    )
    .expect(\"write generated.rs\");
    println!(\"cargo:rustc-env=BUILD_RS_OUT_DIR={out_dir}\");
}
";

/// `a`'s lib (A4): includes the generated file and asserts value, cfg and `OUT_DIR` parity.
const A4_LIB_RS: &str = "// rudzio_test marker
include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));

#[cfg(has_generated)]
pub fn feature_gated() -> u32 {
    7
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn generated_value(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(crate::GENERATED == 99, \"GENERATED should be 99, got {}\", crate::GENERATED);
            Ok(())
        }

        #[rudzio::test]
        fn cfg_is_active(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(crate::feature_gated() == 7);
            Ok(())
        }

        #[rudzio::test]
        fn out_dir_unifies(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                option_env!(\"BUILD_RS_OUT_DIR\") == Some(env!(\"OUT_DIR\")),
                \"build.rs runtime OUT_DIR should equal src env!(OUT_DIR): build={:?} src={}\",
                option_env!(\"BUILD_RS_OUT_DIR\"),
                env!(\"OUT_DIR\")
            );
            Ok(())
        }
    }
}
";

/// A5: build.rs declares a sibling module (`mod helper;`) and a
/// `#[path]`-located module (`build/extra.rs`). Both must resolve
/// relative to the forwarded build.rs's own directory, not the bridge.
const A5_BUILD_RS: &str = "mod helper;
#[path = \"build/extra.rs\"]
mod extra;

fn main() {
    println!(\"cargo:rustc-env=HELPER_SUM={}\", helper::value() + extra::value());
}
";

/// `a`'s sibling build-script module (A5), value 10.
const A5_HELPER_RS: &str = "pub fn value() -> u32 {
    10
}
";

/// `a`'s `#[path]` build-script module (A5), value 20.
const A5_EXTRA_RS: &str = "pub fn value() -> u32 {
    20
}
";

/// `a`'s lib (A5): asserts the module sum reached it as `HELPER_SUM`.
const A5_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn helper_sum(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                option_env!(\"HELPER_SUM\") == Some(\"30\"),
                \"build-script sibling modules should sum to 30, got {:?}\",
                option_env!(\"HELPER_SUM\")
            );
            Ok(())
        }
    }
}
";

/// A6: build.rs builds a real C static lib with the system `cc`/`ar`
/// (the `cc` crate is absent from the offline per-test registry) and emits
/// `cargo:rustc-link-search` + `cargo:rustc-link-lib`. The forwarded
/// build script must run the C compile and the link directives must
/// reach rustc so the `extern \"C\"` symbol resolves.
const A6_HELPER_C: &str = "#include <stdint.h>

int32_t rudzio_helper_add_one(int32_t x) {
    return x + 1;
}
";

/// `a`'s build script (A6): builds `librudzio_helper.a` and emits the link directives.
const A6_BUILD_RS: &str = "use std::process::Command;

fn run(cmd: &mut Command) {
    let status = cmd.status().expect(\"spawn\");
    assert!(status.success(), \"{cmd:?} failed\");
}

fn main() {
    let manifest_dir = std::env::var(\"CARGO_MANIFEST_DIR\").expect(\"CARGO_MANIFEST_DIR\");
    let out_dir = std::env::var(\"OUT_DIR\").expect(\"OUT_DIR\");
    run(Command::new(\"cc\")
        .args([\"-c\", \"-o\"])
        .arg(format!(\"{out_dir}/helper.o\"))
        .arg(format!(\"{manifest_dir}/helper.c\")));
    run(Command::new(\"ar\")
        .arg(\"rcs\")
        .arg(format!(\"{out_dir}/librudzio_helper.a\"))
        .arg(format!(\"{out_dir}/helper.o\")));
    println!(\"cargo:rustc-link-search=native={out_dir}\");
    println!(\"cargo:rustc-link-lib=static=rudzio_helper\");
}
";

/// `a`'s lib (A6): calls the linked C symbol.
const A6_LIB_RS: &str = "// rudzio_test marker
unsafe extern \"C\" {
    fn rudzio_helper_add_one(x: i32) -> i32;
}

#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn linked_c_symbol_resolves(_t: &super::Test) -> anyhow::Result<()> {
            // SAFETY: `rudzio_helper_add_one` is a trivial extern C fn
            // with no preconditions.
            let got = unsafe { crate::rudzio_helper_add_one(41) };
            anyhow::ensure!(got == 42, \"linked C fn should return 42, got {got}\");
            Ok(())
        }
    }
}
";

/// A7: build.rs on a member that build-depends on a NON-rudzio path
/// sibling (a plain lib with no test surface). The build-dep must
/// resolve to the sibling's real lib so the forwarded build script can
/// `use` it.
const A7_SIBLING_LIB_RS: &str = "pub fn compute() -> u32 {
    5
}
";

/// `a`'s build script (A7): calls into the build-dependency sibling `b`.
const A7_BUILD_RS: &str = "fn main() {
    println!(\"cargo:rustc-env=B_COMPUTE={}\", b::compute());
}
";

/// `a`'s lib (A7): asserts `B_COMPUTE` from the build-dep sibling.
const A7_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn build_dep_sibling_resolves(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                option_env!(\"B_COMPUTE\") == Some(\"5\"),
                \"build-dep sibling b::compute() should yield 5, got {:?}\",
                option_env!(\"B_COMPUTE\")
            );
            Ok(())
        }
    }
}
";

/// A8: explicit non-default `package.build = \"tools/gen.rs\"` combined
/// with a registry `[build-dependencies]` entry and a `cargo:rustc-env`
/// emission. The forwarded script must resolve its build-dep and emit
/// the env.
const A8_BUILD_RS: &str = "fn main() -> anyhow::Result<()> {
    println!(\"cargo:rustc-env=CUSTOM_BUILD_WITH_DEP=yes\");
    Ok(())
}
";

/// `d`'s lib (A8): asserts the custom-path build script ran.
const A8_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn custom_build_with_dep(_t: &super::Test) -> anyhow::Result<()> {
            anyhow::ensure!(
                option_env!(\"CUSTOM_BUILD_WITH_DEP\") == Some(\"yes\"),
                \"custom-build script with build-dep did not run\"
            );
            Ok(())
        }
    }
}
";

// ---------------------------------------------------------------------------
// Part B fixture (aggregator lifecycle)
// ---------------------------------------------------------------------------

/// A member `lib.rs` whose only rudzio surface is a single renamed test.
/// Used to assert a consumer source change is picked up on the next run
/// (the previous `trivial_test` must be gone, the new name must appear).
const B2_RENAMED_LIB_RS: &str = "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {
    use rudzio::common::context::{Suite, Test};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {
        #[rudzio::test]
        fn renamed_test(_t: &super::Test) -> anyhow::Result<()> {
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
/// `[dev-dependencies]` (holds `[build-dependencies]`, etc.).
fn manifest(name: &str, sections: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\n\n{sections}[dev-dependencies]\nrudzio = {{ workspace = true }}\nanyhow = {{ workspace = true }}\n"
    )
}

/// Render a member manifest with an explicit `build` script path in
/// `[package]` (a non-default path cargo cannot auto-detect).
fn manifest_with_build(name: &str, build: &str, sections: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\nbuild = \"{build}\"\n\n[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\n\n{sections}[dev-dependencies]\nrudzio = {{ workspace = true }}\nanyhow = {{ workspace = true }}\n"
    )
}

/// Render a plain lib member manifest (no rudzio surface, no
/// `[dev-dependencies]`).
fn plain_lib_manifest(name: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\n"
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
    Ok(
        Command::new(cargo_rudzio_test_helpers::assert_fresh_binary(env!(
            "CARGO_BIN_EXE_cargo-rudzio"
        ))?)
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
        .output()?,
    )
}

/// Spawn `cargo-rudzio generate-runner --output <dir>` from `root`.
fn run_generate_runner(cargo: &CargoEnv<'_>, root: &Path, output: &Path) -> anyhow::Result<Output> {
    Ok(
        Command::new(cargo_rudzio_test_helpers::assert_fresh_binary(env!(
            "CARGO_BIN_EXE_cargo-rudzio"
        ))?)
        .arg("generate-runner")
        .arg("--output")
        .arg(output)
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo.home)
        .env("CARGO_TARGET_DIR", cargo.target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("NO_COLOR", "1")
        .output()?,
    )
}

/// Build a generated aggregator manifest with plain `cargo build`.
fn cargo_build_manifest(
    cargo: &CargoEnv<'_>,
    root: &Path,
    manifest_path: &Path,
) -> anyhow::Result<Output> {
    Ok(
        Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .arg("build")
            .arg("--manifest-path")
            .arg(manifest_path)
            .current_dir(root)
            .env("CARGO_HOME", cargo.home)
            .env("CARGO_TARGET_DIR", cargo.target_dir)
            .env("CARGO_NET_OFFLINE", "true")
            .env("NO_COLOR", "1")
            .output()?,
    )
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

/// Assert the run exited 0 and the passed leaf set equals `expected`
/// exactly.
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
    Ok(())
}

/// Assert the run exited non-zero and stdout+stderr contains every
/// required substring (for loud-failure diagnostics).
fn assert_fails_with(output: &Output, substrings: &[&str]) -> anyhow::Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    anyhow::ensure!(
        !output.status.success(),
        "run was expected to fail but exited 0\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    for needle in substrings {
        anyhow::ensure!(
            combined.contains(needle),
            "diagnostic missing substring {needle:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
    }
    Ok(())
}

/// Locate the unique `rudzio-auto-runner-*` directory under `target_dir`.
fn find_aggregator_subdir(target_dir: &Path) -> anyhow::Result<PathBuf> {
    let matches: Vec<PathBuf> = fs::read_dir(target_dir)?
        .filter_map(io::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rudzio-auto-runner-"))
        })
        .collect();
    anyhow::ensure!(
        matches.len() == 1,
        "expected exactly 1 aggregator subdir under {}; found {}: {:?}",
        target_dir.display(),
        matches.len(),
        matches,
    );
    matches
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("aggregator subdir list unexpectedly empty"))
}

#[rudzio::suite([(runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest)])]
mod tests {
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::{
        A1_BUILD_RS, A2_BUILD_RS, A3_BUILD_RS, A3_LIB_RS, A4_BUILD_RS, A4_LIB_RS, A5_BUILD_RS,
        A5_EXTRA_RS, A5_HELPER_RS, A5_LIB_RS, A6_BUILD_RS, A6_HELPER_C, A6_LIB_RS, A7_BUILD_RS,
        A7_LIB_RS, A7_SIBLING_LIB_RS, A8_BUILD_RS, A8_LIB_RS, B2_RENAMED_LIB_RS, CargoEnv,
        IsolatedCargoTest, assert_exact_passes, assert_fails_with, cargo_build_manifest,
        find_aggregator_subdir, manifest, manifest_with_build, passed_leaf_names,
        plain_lib_manifest, run, run_generate_runner, write_file, write_member, write_workspace,
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

    // -- Part A: build.rs propagation -------------------------------------

    /// A1: an inner-attribute `#![...]` build.rs is rejected loudly with
    /// the member name and the hoist-to-outer fix.
    #[rudzio::test]
    fn inner_attribute_build_rs_fails_loudly(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/a", &manifest("a", ""), A3_LIB_RS)?;
        write_file(root, "crates/a", "build.rs", A1_BUILD_RS)?;
        assert_fails_with(
            &run(&env, root, &[])?,
            &[
                "member `a`",
                "begins with an inner attribute",
                "Hoist the attribute to outer form",
            ],
        )
    }

    /// A2: an inner doc comment `//!` build.rs is rejected loudly with
    /// the plain-comment fix.
    #[rudzio::test]
    fn inner_doc_comment_build_rs_fails_loudly(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/a", &manifest("a", ""), A3_LIB_RS)?;
        write_file(root, "crates/a", "build.rs", A2_BUILD_RS)?;
        assert_fails_with(
            &run(&env, root, &[])?,
            &[
                "member `a`",
                "begins with an inner doc comment",
                "Convert them to plain",
            ],
        )
    }

    /// A3: compile-time `env!("CARGO_MANIFEST_DIR")` resolves to the
    /// bridge dir while runtime `std::env::var` is repointed to the
    /// member dir.
    #[rudzio::test]
    fn compile_time_manifest_dir_diverges_to_bridge(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/a", &manifest("a", ""), A3_LIB_RS)?;
        write_file(root, "crates/a", "build.rs", A3_BUILD_RS)?;
        assert_exact_passes(
            &run(&env, root, &[])?,
            &["manifest_dir_runtime_vs_compiletime"],
        )
    }

    /// A4: the build script's runtime `OUT_DIR` unifies with the src's, the
    /// generated file reaches src, and cfg + check-cfg are honoured.
    #[rudzio::test]
    fn build_script_out_dir_unifies_with_src(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/a", &manifest("a", ""), A4_LIB_RS)?;
        write_file(root, "crates/a", "build.rs", A4_BUILD_RS)?;
        assert_exact_passes(
            &run(&env, root, &[])?,
            &["generated_value", "cfg_is_active", "out_dir_unifies"],
        )
    }

    /// A5: `mod helper;` and `#[path]` modules resolve relative to the
    /// forwarded build.rs's own directory.
    #[rudzio::test]
    fn sibling_and_path_build_rs_modules_resolve(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/a", &manifest("a", ""), A5_LIB_RS)?;
        write_file(root, "crates/a", "build.rs", A5_BUILD_RS)?;
        write_file(root, "crates/a", "helper.rs", A5_HELPER_RS)?;
        write_file(root, "crates/a/build", "extra.rs", A5_EXTRA_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["helper_sum"])
    }

    /// A6: a `cc`-compiled C static lib linked via forwarded
    /// `cargo:rustc-link-*` directives resolves at link time.
    #[rudzio::test]
    fn link_lib_and_link_search_directives_honoured(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(root, "crates/a", &manifest("a", ""), A6_LIB_RS)?;
        write_file(root, "crates/a", "build.rs", A6_BUILD_RS)?;
        write_file(root, "crates/a", "helper.c", A6_HELPER_C)?;
        assert_exact_passes(&run(&env, root, &[])?, &["linked_c_symbol_resolves"])
    }

    /// A7: a build-dependency on a NON-rudzio path sibling resolves to
    /// the sibling's real lib.
    #[rudzio::test]
    fn build_dep_on_non_rudzio_sibling_resolves(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/b",
            &plain_lib_manifest("b"),
            A7_SIBLING_LIB_RS,
        )?;
        write_member(
            root,
            "crates/a",
            &manifest("a", "[build-dependencies]\nb = { path = \"../b\" }\n"),
            A7_LIB_RS,
        )?;
        write_file(root, "crates/a", "build.rs", A7_BUILD_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["build_dep_sibling_resolves"])
    }

    /// A8: explicit `package.build` + registry `[build-dependencies]` +
    /// `cargo:rustc-env` all forwarded together.
    #[rudzio::test]
    fn custom_build_path_with_build_dep_and_env(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace = TempDir::new_in(env.tempdir)?;
        let root = workspace.path();
        write_workspace(root, "[\"crates/*\"]")?;
        write_member(
            root,
            "crates/d",
            &manifest_with_build(
                "d",
                "tools/gen.rs",
                "[build-dependencies]\nanyhow = \"=1.0.104\"\n",
            ),
            A8_LIB_RS,
        )?;
        write_file(root, "crates/d/tools", "gen.rs", A8_BUILD_RS)?;
        assert_exact_passes(&run(&env, root, &[])?, &["custom_build_with_dep"])
    }

    // -- Part B: aggregator lifecycle / cache -----------------------------

    /// B1: two sequential runs against the same workspace reuse the same
    /// aggregator subdir (stable cache key) and both pass.
    #[rudzio::test]
    fn second_run_reuses_same_aggregator_subdir(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace: &Path = ctx.consumer_workspace();
        assert_exact_passes(&run(&env, workspace, &[])?, &["trivial_test"])?;
        let first_subdir = find_aggregator_subdir(env.target_dir)?;
        assert_exact_passes(&run(&env, workspace, &[])?, &["trivial_test"])?;
        let second_subdir = find_aggregator_subdir(env.target_dir)?;
        anyhow::ensure!(
            first_subdir == second_subdir,
            "second run must reuse the same aggregator subdir; first={} second={}",
            first_subdir.display(),
            second_subdir.display(),
        );
        Ok(())
    }

    /// B2: a consumer source change (test renamed) is picked up on the
    /// next run.
    #[rudzio::test]
    fn consumer_source_change_picked_up_next_run(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace: &Path = ctx.consumer_workspace();
        assert_exact_passes(&run(&env, workspace, &[])?, &["trivial_test"])?;
        let lib_rs = workspace.join("consumer").join("src").join("lib.rs");
        fs::write(&lib_rs, B2_RENAMED_LIB_RS)?;
        assert_exact_passes(&run(&env, workspace, &[])?, &["renamed_test"])?;
        Ok(())
    }

    /// B3: two concurrent `cargo rudzio test` invocations against the
    /// same workspace with DIFFERENT task-local `CARGO_TARGET_DIR`s both
    /// succeed (isolation).
    #[rudzio::test]
    fn concurrent_distinct_target_dirs_both_succeed(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace: &Path = ctx.consumer_workspace();
        let target_a = env.tempdir.join("concurrent-a-target");
        let target_b = env.tempdir.join("concurrent-b-target");
        fs::create_dir_all(&target_a)?;
        fs::create_dir_all(&target_b)?;

        let bin =
            cargo_rudzio_test_helpers::assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;
        let mut children = Vec::new();
        for target in [&target_a, &target_b] {
            let child = Command::new(&bin)
                .arg("test")
                .current_dir(workspace)
                .env(
                    "CARGO",
                    env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
                )
                .env("CARGO_HOME", env.home)
                .env("CARGO_TARGET_DIR", target)
                .env("CARGO_NET_OFFLINE", "true")
                .env("NO_COLOR", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            children.push(child);
        }
        let mut outputs = Vec::new();
        for child in children {
            outputs.push(child.wait_with_output()?);
        }
        for output in &outputs {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::ensure!(
                output.status.success(),
                "concurrent invocation exited {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
                output.status.code(),
            );
            anyhow::ensure!(
                passed_leaf_names(&stdout).contains("trivial_test"),
                "concurrent invocation did not report trivial_test\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            );
        }
        Ok(())
    }

    /// B4: `generate-runner --output DIR` writes a buildable aggregator
    /// that reports the same tests when built and run.
    #[rudzio::test]
    fn generate_runner_output_builds_and_runs(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let env = cargo_env!(ctx);
        let workspace: &Path = ctx.consumer_workspace();
        let out_dir = env.tempdir.join("generated-runner");
        let gen_out = run_generate_runner(&env, workspace, &out_dir)?;
        let gen_stdout = String::from_utf8_lossy(&gen_out.stdout);
        let gen_stderr = String::from_utf8_lossy(&gen_out.stderr);
        anyhow::ensure!(
            gen_out.status.success(),
            "generate-runner exited {:?}\n--- stdout ---\n{gen_stdout}\n--- stderr ---\n{gen_stderr}",
            gen_out.status.code(),
        );
        anyhow::ensure!(
            out_dir.join("Cargo.toml").is_file(),
            "generate-runner did not write Cargo.toml under {}",
            out_dir.display(),
        );

        let manifest_path = out_dir.join("Cargo.toml");
        let build_output = cargo_build_manifest(&env, workspace, &manifest_path)?;
        let build_stdout = String::from_utf8_lossy(&build_output.stdout);
        let build_stderr = String::from_utf8_lossy(&build_output.stderr);
        anyhow::ensure!(
            build_output.status.success(),
            "cargo build of generated aggregator exited {:?}\n--- stdout ---\n{build_stdout}\n--- stderr ---\n{build_stderr}",
            build_output.status.code(),
        );

        let runner = env.target_dir.join("debug").join("rudzio-auto-runner");
        anyhow::ensure!(
            runner.is_file(),
            "expected runner binary at {}",
            runner.display(),
        );
        let run_out = Command::new(&runner).env("NO_COLOR", "1").output()?;
        let run_stdout = String::from_utf8_lossy(&run_out.stdout);
        let run_stderr = String::from_utf8_lossy(&run_out.stderr);
        anyhow::ensure!(
            run_out.status.success(),
            "runner exited {:?}\n--- stdout ---\n{run_stdout}\n--- stderr ---\n{run_stderr}",
            run_out.status.code(),
        );
        anyhow::ensure!(
            passed_leaf_names(&run_stdout).contains("trivial_test"),
            "runner did not report trivial_test\n--- stdout ---\n{run_stdout}\n--- stderr ---\n{run_stderr}",
        );
        Ok(())
    }
}
