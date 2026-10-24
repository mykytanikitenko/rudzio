//! Regression matrix for migration Bug #2: `[lib] harness = false` and
//! `cargo clippy --all-targets --all-features -- -D warnings`.
//!
//! # Reported symptom
//!
//! A crate following the README's "Lib unit tests (no `tests/`
//! directory)" recipe — `[lib] harness = false` plus a
//! `#[cfg(test)] #[rudzio::main] fn main() {}` entry point — was
//! reported to fail `cargo clippy --all-targets --all-features -- -D
//! warnings` with `error[E0601]: main function not found in crate`.
//!
//! # Finding (toolchain 1.99.1)
//!
//! The documented shapes do NOT reproduce E0601. `clippy --all-targets`
//! compiles the lib's `harness = false` test target with `cfg(test)`
//! active, so a `#[cfg(test)]` `fn main` at the crate root registers and
//! the target links. Both the README recipe (shape A) and the
//! `rudzio-migrate` output (shape C — `[lib] harness = false` *plus*
//! `[lib] test = false` and the `unexpected_cfgs` check-cfg) pass
//! `clippy --all-targets --all-features -- -D warnings` with exit 0.
//!
//! E0601 arises only when the entry point does not resolve to a
//! crate-root `fn main` under the linted target's cfg set:
//!
//! * a `#[rudzio::main] fn main` nested inside a `#[cfg(test)] mod`
//!   (it becomes `tests::main`, not `crate::main`), or
//! * a `fn main` gated behind `#[cfg(all(test, feature = "x"))]` when
//!   the feature is not enabled (plain `--all-targets` without
//!   `--all-features`).
//!
//! Neither shape is produced by the README recipe or by
//! `rudzio-migrate` (`migrate/src/run.rs::ensure_lib_has_rudzio_main`
//! appends the entry point at the crate root). This spec is therefore a
//! regression *matrix* asserting the documented shapes stay clippy-clean,
//! not a reproduction of a defect in the current toolchain.
//!
//! # Assertions
//!
//! Each test spawns `cargo clippy --all-targets --all-features -- -D
//! warnings` in a scratch crate and asserts exit 0.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repository root. This spec compiles inside the `cargo-rudzio` test
/// binary, so `env!("CARGO_MANIFEST_DIR")` resolves to
/// `<repo>/cargo-rudzio` and its parent is the repo root the scratch
/// crate's rudzio `path` dep must point at.
fn repo_root() -> anyhow::Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent().ok_or_else(|| {
        anyhow::anyhow!("cargo-rudzio lives at <repo>/cargo-rudzio; its parent is the repo root")
    })?;
    Ok(root.to_path_buf())
}

/// Shared `clippy --all-targets --all-features -- -D warnings` runner.
/// Every matrix shape funnels through this so the exact invocation stays
/// identical across shapes.
fn run_clippy_all_targets(
    scratch: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
) -> anyhow::Result<()> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo_bin)
        .arg("clippy")
        .arg("--all-targets")
        .arg("--all-features")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .current_dir(scratch)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "`cargo clippy --all-targets --all-features -- -D warnings` failed \
         (status {:?}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// Shape A — the README-documented `[lib] harness = false` recipe with a
/// crate-root `#[cfg(test)] #[rudzio::main] fn main() {}`.
fn write_fixture_readme(root: &Path, rudzio_root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "lib_unit_harness_false"
version = "0.1.0"
edition = "2024"

[lib]
harness = false

[dev-dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        root.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::*;

    #[rudzio::test]
    async fn sums_correctly() -> anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        Ok(())
    }
}

#[cfg(test)]
#[rudzio::main]
fn main() {}
",
    )?;
    Ok(())
}

/// Shape C — the manifest and lib.rs emitted by `rudzio-migrate` for a
/// lib crate with `src/` unit tests (`migrate/fixtures/lib_unit_tests_harness_false`):
/// `[lib] harness = false` *and* `[lib] test = false` plus the
/// `unexpected_cfgs` check-cfg, with the `#[cfg(test)] #[::rudzio::main]`
/// entry point still at the crate root.
fn write_fixture_migrated(root: &Path, rudzio_root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "lib_unit_migrated"
version = "0.1.0"
edition = "2024"
autotests = false

[lib]
harness = false
test = false

[dev-dependencies]
rudzio = {{ path = "{rudzio}", features = ["common", "runtime-tokio-multi-thread"] }}
anyhow = "=1.0.104"

[lints.rust]
unexpected_cfgs = {{ level = "warn", check-cfg = ["cfg(rudzio_test)"] }}
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        root.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(any(test, rudzio_test))]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),]
)]
mod tests {
    use super::*;
    #[::rudzio::test]
    async fn sums_correctly() {
        assert_eq!(add(1, 2), 3);
    }
}

#[cfg(test)]
#[::rudzio::main]
fn main() {}
",
    )?;
    Ok(())
}

/// Shape B1 — `[lib] harness = false` with the `#[rudzio::main]` entry
/// point gated behind a non-default feature. Under `--all-features` the
/// feature is enabled, so the crate-root `fn main` registers and clippy
/// passes.
fn write_fixture_feature_gated(root: &Path, rudzio_root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "lib_unit_feature_gated_main"
version = "0.1.0"
edition = "2024"

[features]
default = []
x = []

[lib]
harness = false

[dev-dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        root.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::*;

    #[rudzio::test]
    async fn sums_correctly() -> anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        Ok(())
    }
}

#[cfg(all(test, feature = \"x\"))]
#[rudzio::main]
fn main() {}
",
    )?;
    Ok(())
}

/// Shape D — the operator's lib + `[[bin]]` `token` shape, with the
/// CORRECT recipe. The lib uses `[lib] harness = false` plus a
/// crate-root `#[cfg(test)] #[rudzio::main] fn main() {}` (shape A).
/// The `token` bin keeps a real, un-gated `fn main`: `clippy
/// --all-targets` compiles every `[[bin]]` target without `cfg(test)`,
/// so a gated (or missing) `fn main` yields E0601. `[[bin]] test =
/// false` (NOT `harness = false`) suppresses the bin's libtest pass.
fn write_fixture_lib_bin_token(root: &Path, rudzio_root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root.join("src/bin"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "lib_token_bin"
version = "0.1.0"
edition = "2024"

[lib]
harness = false

[[bin]]
name = "token"
test = false

[dev-dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        root.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::*;

    #[rudzio::test]
    async fn sums_correctly() -> anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        Ok(())
    }
}

#[cfg(test)]
#[rudzio::main]
fn main() {}
",
    )?;
    fs::write(
        root.join("src/bin/token.rs"),
        "// The `token` bin keeps a real, un-gated `fn main`. `clippy
// --all-targets` (and `cargo build --bins`) compile the bin target
// without `cfg(test)`, so a `#[cfg(test)]`-gated entry point here
// would leave the bin without a `main` and fail E0601.
fn main() {
    println!(\"token\");
}
",
    )?;
    Ok(())
}

/// Shape D-broken — the operator's misconfigured `token` bin:
/// `[[bin]] harness = false` (the wrong key for a bin) plus a
/// `#[cfg(test)]`-gated entry point. `clippy --all-targets` compiles
/// the bin target without `cfg(test)`, so `fn main` is absent and the
/// build fails `E0601`. This pins the exact failure the README now
/// warns against.
fn write_fixture_broken_token_bin(root: &Path, rudzio_root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root.join("src/bin"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "broken_token_bin"
version = "0.1.0"
edition = "2024"

[lib]
harness = false

[[bin]]
name = "token"
harness = false

[dev-dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            rudzio = rudzio_root.display(),
        ),
    )?;
    fs::write(
        root.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
#[rudzio::main]
fn main() {}
",
    )?;
    fs::write(
        root.join("src/bin/token.rs"),
        "// Broken: the entry point is `#[cfg(test)]`-gated, so the bin
// target (compiled by `clippy --all-targets` without `cfg(test)`) has
// no `fn main` and rustc rejects it with E0601.
#[cfg(test)]
#[rudzio::main]
fn main() {}
",
    )?;
    Ok(())
}

/// Shared `clippy --all-targets --all-features -- -D warnings` runner
/// that expects a FAILURE. Returns the captured stderr so the caller
/// can assert the exact `E0601` diagnostic.
fn run_clippy_all_targets_expect_failure(
    scratch: &Path,
    cargo_home: &Path,
    cargo_target_dir: &Path,
) -> anyhow::Result<String> {
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo_bin)
        .arg("clippy")
        .arg("--all-targets")
        .arg("--all-features")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .current_dir(scratch)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .output()?;
    anyhow::ensure!(
        !output.status.success(),
        "expected `cargo clippy --all-targets --all-features -- -D warnings` to fail, \
         but it succeeded",
    );
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use super::{
        IsolatedCargoTest, TempDir, repo_root, run_clippy_all_targets,
        run_clippy_all_targets_expect_failure, write_fixture_broken_token_bin,
        write_fixture_feature_gated, write_fixture_lib_bin_token, write_fixture_migrated,
        write_fixture_readme,
    };

    /// Bug #2, shape A — the README `[lib] harness = false` recipe must
    /// not break `cargo clippy --all-targets --all-features`.
    #[rudzio::test]
    fn lib_harness_false_clippy_all_targets_succeeds(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let scratch = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture_readme(scratch.path(), &repo_root()?)?;
        run_clippy_all_targets(scratch.path(), ctx.cargo_home(), ctx.cargo_target_dir())
    }

    /// Bug #2, shape C — the `rudzio-migrate` output (`[lib] harness =
    /// false` + `[lib] test = false` + check-cfg) must not break
    /// `cargo clippy --all-targets --all-features`.
    #[rudzio::test]
    fn migrated_lib_test_false_clippy_all_targets_succeeds(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let scratch = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture_migrated(scratch.path(), &repo_root()?)?;
        run_clippy_all_targets(scratch.path(), ctx.cargo_home(), ctx.cargo_target_dir())
    }

    /// Bug #2, shape B1 — a feature-gated `#[rudzio::main]` is present
    /// under `--all-features`, so `cargo clippy --all-targets
    /// --all-features` must stay green.
    #[rudzio::test]
    fn feature_gated_main_clippy_all_features_succeeds(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let scratch = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture_feature_gated(scratch.path(), &repo_root()?)?;
        run_clippy_all_targets(scratch.path(), ctx.cargo_home(), ctx.cargo_target_dir())
    }

    /// Bug #2, shape D — the operator's lib + `[[bin]]` `token` shape,
    /// with the CORRECT recipe (the bin keeps a real, un-gated
    /// `#[rudzio::main] fn main() {}` and `[[bin]] test = false`), must
    /// not break `cargo clippy --all-targets --all-features`.
    #[rudzio::test]
    fn lib_harness_false_with_token_bin_clippy_all_targets_succeeds(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let scratch = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture_lib_bin_token(scratch.path(), &repo_root()?)?;
        run_clippy_all_targets(scratch.path(), ctx.cargo_home(), ctx.cargo_target_dir())
    }

    /// Bug #2, shape D-broken — a `[[bin]] harness = false` with a
    /// `#[cfg(test)]`-gated entry point (no un-gated `fn main`) must
    /// fail `cargo clippy --all-targets` with `E0601: main function
    /// not found in crate 'token'`. Pins the failure the README now
    /// warns against.
    #[rudzio::test]
    fn token_bin_with_cfg_test_only_main_fails_clippy_e0601(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let scratch = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture_broken_token_bin(scratch.path(), &repo_root()?)?;
        let stderr = run_clippy_all_targets_expect_failure(
            scratch.path(),
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
        )?;
        anyhow::ensure!(
            stderr.contains("function not found in crate `token`"),
            "expected E0601 `main function not found in crate 'token'` in stderr;\n--- stderr ---\n{stderr}",
        );
        Ok(())
    }
}
