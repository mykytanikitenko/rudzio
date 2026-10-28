//! Edge-case e2e tests for how `cargo rudzio test` propagates the
//! **consumer's** cargo CONFIGURATION into the inner aggregator build.
//!
//! Bug #4 lived in this seam: `.cargo/config.toml` outranks the manifest
//! `[profile]`, and an ancestor config `panic = "unwind"` silently beat the
//! consumer's `panic = "abort"`. The fix mirrors `panic` onto the inner cargo
//! via `--config profile.<p>.panic=` (`main.rs::panic_profile_overrides`,
//! `cli.rs::aggregator_cargo_args`) and anchors the inner build at the
//! consumer workspace root via `Command::current_dir`. Only `panic` is
//! mirrored; every other profile key and every other config class
//! (`[env]`, `[build] rustflags`, `[target.*] rustflags`, `RUSTFLAGS`,
//! `CARGO_PROFILE_*`, `[source]`, `--config`) relies on cargo's own config
//! discovery walking up from the anchored CWD — so each is a candidate for
//! the SAME class of bug.
//!
//! # Contract under test
//!
//! For each case, `cargo rudzio test` must produce the same effective
//! config that a stock `cargo test -p <member>` run from the same workspace
//! (and same ancestor placement) would. The assertion encodes the observed
//! stock-cargo behaviour; a divergence is a regression.
//!
//! Both placements are exercised where config precedence matters:
//! (a) the fixture in a plain tmp dir (no ancestor config), and (b) the
//! fixture placed UNDER a parent dir carrying a `.cargo/config.toml` that
//! contradicts the consumer's setting — the exact shape that exposed #4
//! (the rudzio repo's own config pins `panic = "unwind"`).
//!
//! # Case 10 (`[alias]` / `[net] offline` / `[source]`) — scoped out
//!
//! * `[alias]` is a no-op for cargo-rudzio: it spawns `cargo build`
//!   directly, and `[alias]` only defines NEW subcommand names (cargo
//!   refuses to alias built-ins like `build`), so there is no alias
//!   surface on the inner build.
//! * `[net] offline = true` (config) is exercised transitively by every
//!   test here: the harness pins `CARGO_NET_OFFLINE=true` (a
//!   higher-precedence env layer than `[net]`), so the inner build runs
//!   offline. A config-level `[net]` has no stronger positive assertion
//!   that can be built without network.
//! * `[source]` replacement requires a full `cargo vendor` tree (all
//!   crates-io deps: `anyhow` + `tokio` + every transitive dep `rudzio`
//!   pulls), which is infeasible to hand-write and requires network to
//!   generate. Skipped per the task's "skip the sub-case and say why"
//!   allowance.

#![cfg(unix)]

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest, assert_fresh_binary};
use rudzio::runtime::tokio::Multithread;

/// Repo root = parent of `cargo-rudzio/` (where this spec lives). Used as
/// the `path` for the fixture's `rudzio` workspace dependency.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Write a single-member (`app`) workspace at `root`.
///
/// `profile` is the raw `[profile.*]` TOML embedded in the root manifest
/// (empty to omit profiles). `consumer_config` is the optional content of
/// `<root>/.cargo/config.toml`. `test_name` / `test_body` splice the single
/// `#[rudzio::test]` fn verbatim.
fn write_fixture(
    root: &Path,
    rudzio_root: &Path,
    profile: &str,
    consumer_config: Option<&str>,
    test_name: &str,
    test_body: &str,
) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["app"]
resolver = "3"

{profile}[workspace.dependencies]
rudzio = {{ path = "{rudzio}", features = ["runtime-tokio-multi-thread", "common"] }}
anyhow = "=1.0.104"
"#,
            profile = profile,
            rudzio = rudzio_root.display(),
        ),
    )?;

    let app = root.join("app");
    fs::create_dir_all(app.join("src"))?;
    fs::write(
        app.join("Cargo.toml"),
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
name = "app"
path = "src/lib.rs"

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
"#,
    )?;

    fs::write(
        app.join("src").join("lib.rs"),
        format!(
            "// rudzio_test marker\n\
             #[cfg(any(test, rudzio_test))]\n\
             mod tests {{\n\
             \x20\x20\x20\x20use rudzio::common::context::{{Suite, Test}};\n\
             \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
             \n\
             \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
             \x20\x20\x20\x20mod inner {{\n\
             \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
             \x20\x20\x20\x20\x20\x20\x20\x20fn {test_name}(_t: &super::Test) -> anyhow::Result<()> {{\n\
             {test_body}\n\
             \x20\x20\x20\x20\x20\x20\x20\x20}}\n\
             \x20\x20\x20\x20}}\n\
             }}\n"
        ),
    )?;

    if let Some(config) = consumer_config {
        fs::create_dir_all(root.join(".cargo"))?;
        fs::write(root.join(".cargo").join("config.toml"), config)?;
    }

    Ok(())
}

/// Write an ancestor `.cargo/config.toml` at `<parent>/.cargo/config.toml`.
fn write_ancestor_config(parent: &Path, config: &str) -> io::Result<()> {
    fs::create_dir_all(parent.join(".cargo"))?;
    fs::write(parent.join(".cargo").join("config.toml"), config)
}

/// Drive `cargo rudzio test` against the fixture at `root` with the
/// isolated cargo env, forwarding `extra_args` and `extra_env`.
fn run_cargo_rudzio(
    cargo_home: &Path,
    cargo_target_dir: &Path,
    cargo_build_dir: &Path,
    root: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) -> anyhow::Result<Output> {
    let binary = assert_fresh_binary(env!("CARGO_BIN_EXE_cargo-rudzio"))?;
    let mut cmd = Command::new(binary);
    let _: &mut Command = cmd
        .arg("test")
        .args(extra_args)
        .current_dir(root)
        .env(
            "CARGO",
            env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        )
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("CARGO_BUILD_BUILD_DIR", cargo_build_dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTC_WRAPPER", "")
        .env("SCCACHE_DISABLE", "1")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        let _: &mut Command = cmd.env(key, value);
    }
    let output = cmd.output().map_err(io::Error::other)?;
    Ok(output)
}

#[rudzio::suite([(
    runtime = Multithread::new,
    suite = IsolatedCargoSuite,
    test = IsolatedCargoTest,
)])]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::{
        IsolatedCargoTest, Output, Path, PathBuf, env, fs, rudzio_source_root, run_cargo_rudzio,
        write_ancestor_config, write_fixture,
    };

    /// Repo root of the rudzio workspace under test (parent of `cargo-rudzio/`).
    fn rudzio_root() -> anyhow::Result<&'static Path> {
        rudzio_source_root()
            .ok_or_else(|| anyhow::anyhow!("cargo-rudzio/tests must live under the workspace root"))
    }

    /// Per-test build dir (`CARGO_BUILD_BUILD_DIR`) as a sibling of the
    /// per-test target dir, mirroring the isolated-cargo harness layout.
    fn cargo_build_dir_for(cargo_target_dir: &Path) -> anyhow::Result<PathBuf> {
        Ok(cargo_target_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cargo target dir has a parent"))?
            .join("cargo-build-dir"))
    }

    /// stdout + stderr of a spawned subprocess, joined for assertion greps.
    fn combined(output: &Output) -> String {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// Assert a cargo-rudzio run passed and reported `test_name` (the exact
    /// passed-test-name, not just a pass count).
    fn assert_passed(output: &Output, test_name: &str) -> anyhow::Result<()> {
        let text = combined(output);
        anyhow::ensure!(
            output.status.success(),
            "expected the test to pass, but cargo rudzio exited {:?}.\n{text}",
            output.status.code(),
        );
        anyhow::ensure!(
            text.contains(test_name),
            "expected the passed-test-name `{test_name}` in output:\n{text}",
        );
        anyhow::ensure!(text.contains("1 passed"), "output:\n{text}");
        Ok(())
    }

    // ── 1. `[profile.dev] debug-assertions` + `overflow-checks` ─────────
    //
    // Stock cargo: the ancestor `.cargo/config.toml` `[profile.dev]` table
    // outranks the manifest `[profile.dev]`. So under the ancestor placement
    // the effective values are the ancestor's (true/true); in a plain dir the
    // manifest's (false/false) hold. cargo-rudzio must match both.

    #[rudzio::test]
    fn debug_assertions_ancestor_config_outranks_manifest(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let parent = ctx.suite_tempdir().join("case1-ancestor");
        let root = parent.join("ws");
        fs::create_dir_all(&root)?;
        write_ancestor_config(
            &parent,
            "[profile.dev]\ndebug-assertions = true\noverflow-checks = true\n",
        )?;
        write_fixture(
            &root,
            rudzio_root()?,
            "[profile.dev]\ndebug-assertions = false\noverflow-checks = false\n",
            None,
            "debug_assertions_ancestor_wins",
            r#"            anyhow::ensure!(cfg!(debug_assertions), "ancestor config should turn debug-assertions ON");
            let panicked = std::panic::catch_unwind(|| {
                let _ = std::hint::black_box(200u8) * std::hint::black_box(2u8);
            })
            .is_err();
            anyhow::ensure!(panicked, "ancestor config should turn overflow-checks ON (overflow must panic)");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "debug_assertions_ancestor_wins")
    }

    #[rudzio::test]
    fn debug_assertions_plain_placement_follows_manifest(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case1-plain");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "[profile.dev]\ndebug-assertions = false\noverflow-checks = false\n",
            None,
            "debug_assertions_manifest_wins",
            r#"            anyhow::ensure!(!cfg!(debug_assertions), "manifest should turn debug-assertions OFF");
            let wrapped = std::panic::catch_unwind(|| {
                let _ = std::hint::black_box(200u8) * std::hint::black_box(2u8);
            })
            .is_ok();
            anyhow::ensure!(wrapped, "manifest should turn overflow-checks OFF (overflow must wrap, not panic)");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "debug_assertions_manifest_wins")
    }

    // ── 3. `[env] MY_VAR` ───────────────────────────────────────────────
    //
    // Stock cargo sets `[env]` vars for both rustc (so `option_env!` sees
    // them) and the process it runs (so `std::env::var` sees them at
    // runtime). The inner build is anchored at the consumer root, so rustc
    // (compile-time) sees it. The runner, however, is `exec`d by
    // cargo-rudzio directly — NOT by cargo — so the runtime case is a
    // candidate divergence. Two separate tests isolate them.

    #[rudzio::test]
    fn config_env_var_reaches_compile_time(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case3-env-opt");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            Some("[env]\nMY_VAR = \"consumer\"\n"),
            "config_env_compile_time",
            r#"            anyhow::ensure!(option_env!("MY_VAR") == Some("consumer"), "consumer [env] MY_VAR must reach rustc via option_env");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "config_env_compile_time")
    }

    #[rudzio::test]
    fn config_env_var_reaches_runtime(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case3-env-runtime");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            Some("[env]\nMY_VAR = \"consumer\"\n"),
            "config_env_runtime",
            r#"            anyhow::ensure!(std::env::var("MY_VAR").as_deref() == Ok("consumer"), "consumer [env] MY_VAR must reach the runner at runtime");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "config_env_runtime")
    }

    #[rudzio::test]
    fn config_env_extensionless_config_reaches_runtime(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case3-env-extless");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            None,
            "config_env_extensionless",
            r#"            anyhow::ensure!(std::env::var("MY_VAR").as_deref() == Ok("legacy"), "extensionless .cargo/config [env] MY_VAR must reach the runner at runtime");
            Ok(())
"#,
        )?;
        // Both files present: cargo prefers the extensionless `config`.
        fs::create_dir_all(root.join(".cargo"))?;
        fs::write(
            root.join(".cargo").join("config"),
            "[env]\nMY_VAR = \"legacy\"\n",
        )?;
        fs::write(
            root.join(".cargo").join("config.toml"),
            "[env]\nMY_VAR = \"modern\"\n",
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "config_env_extensionless")
    }

    #[rudzio::test]
    fn config_env_cli_config_override_reaches_runtime(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case3-env-cli");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            None,
            "config_env_cli_override",
            r#"            anyhow::ensure!(std::env::var("MY_VAR").as_deref() == Ok("cli"), "--config env.MY_VAR must reach the runner at runtime");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &["--config", "env.MY_VAR=\"cli\""],
            &[],
        )?;
        assert_passed(&output, "config_env_cli_override")
    }

    #[rudzio::test]
    fn config_env_cli_config_file_reaches_runtime(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case3-env-cli-file");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            None,
            "config_env_cli_file",
            r#"            anyhow::ensure!(std::env::var("MY_VAR").as_deref() == Ok("from-file"), "--config <path> [env] MY_VAR must reach the runner at runtime");
            Ok(())
"#,
        )?;
        let extra = root.join("extra-config.toml");
        fs::write(&extra, "[env]\nMY_VAR = \"from-file\"\n")?;
        let extra_arg = extra
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("config path is not UTF-8"))?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &["--config", extra_arg],
            &[],
        )?;
        assert_passed(&output, "config_env_cli_file")
    }

    #[rudzio::test]
    fn config_env_cargo_home_fallback_and_workspace_outranks(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case3-env-precedence");
        fs::create_dir_all(&root)?;
        // Lower-precedence layer: `$CARGO_HOME/config.toml` (the isolated
        // per-test home `run_cargo_rudzio` passes as CARGO_HOME).
        fs::write(
            ctx.cargo_home().join("config.toml"),
            "[env]\nMY_VAR = \"home\"\nHOME_ONLY = \"home-only\"\n",
        )?;
        // Higher-precedence layer: the consumer workspace's own config.
        // Cargo prefers the closer (workspace) config over $CARGO_HOME, and
        // still applies $CARGO_HOME keys the workspace does not shadow.
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            Some("[env]\nMY_VAR = \"consumer\"\nWORKSPACE_ONLY = \"ws-only\"\n"),
            "config_env_precedence",
            r#"            anyhow::ensure!(std::env::var("MY_VAR").as_deref() == Ok("consumer"), "workspace [env] must outrank $CARGO_HOME [env]");
            anyhow::ensure!(std::env::var("WORKSPACE_ONLY").as_deref() == Ok("ws-only"), "workspace [env] WORKSPACE_ONLY must reach the runner");
            anyhow::ensure!(std::env::var("HOME_ONLY").as_deref() == Ok("home-only"), "$CARGO_HOME [env] HOME_ONLY must apply as the fallback layer");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "config_env_precedence")
    }

    // ── 4. `[build] rustflags = ["--cfg", ...]` ─────────────────────────

    #[rudzio::test]
    fn config_build_rustflags_cfg_reaches_test_code(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case4-rustflags");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            Some(
                "[build]\nrustflags = [\"--cfg\", \"consumer_flag\", \"--check-cfg=cfg(consumer_flag)\"]\n",
            ),
            "config_build_rustflags_cfg",
            r#"            anyhow::ensure!(cfg!(consumer_flag), "consumer [build] rustflags --cfg must reach test code");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "config_build_rustflags_cfg")
    }

    // ── 5. `[target.<triple>] rustflags` ────────────────────────────────

    #[rudzio::test]
    fn config_target_specific_rustflags_reaches_test_code(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        // `HOST` is set at runtime by cargo for the process it runs: the
        // host triple this test binary was built for — exactly the triple the
        // consumer's `[target.<triple>]` table must key on.
        let host = env::var("HOST").unwrap_or_else(|_| "x86_64-unknown-linux-gnu".to_owned());
        let root = ctx.suite_tempdir().join("case5-target-rustflags");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            Some(&format!(
                "[target.{host}]\nrustflags = [\"--cfg\", \"target_flag\", \"--check-cfg=cfg(target_flag)\"]\n"
            )),
            "config_target_rustflags_cfg",
            r#"            anyhow::ensure!(cfg!(target_flag), "consumer [target.<triple>] rustflags --cfg must reach test code");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "config_target_rustflags_cfg")
    }

    // ── 6. `RUSTFLAGS` env var set by the caller ─────────────────────────

    #[rudzio::test]
    fn rustflags_env_reaches_test_code(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case6-rustflags-env");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            None,
            "rustflags_env_cfg",
            r#"            anyhow::ensure!(cfg!(env_flag), "RUSTFLAGS from the caller must reach test code");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[("RUSTFLAGS", "--cfg env_flag --check-cfg=cfg(env_flag)")],
        )?;
        assert_passed(&output, "rustflags_env_cfg")
    }

    // ── 7. `CARGO_PROFILE_DEV_PANIC=abort` env (no manifest panic) ──────

    #[rudzio::test]
    fn cargo_profile_dev_panic_env_aborts_runner(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case7-env-panic");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            None,
            "env_panic_aborts",
            "            panic!(\"boom\")\n",
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[("CARGO_PROFILE_DEV_PANIC", "abort")],
        )?;
        let status = output.status;
        let aborted =
            status.signal() == Some(6_i32) || (!status.success() && status.code().is_none());
        anyhow::ensure!(
            aborted,
            "expected CARGO_PROFILE_DEV_PANIC=abort to abort the runner, but it exited {status:?} \
             (code={:?}, signal={:?}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status.code(),
            status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }

    // ── 8. custom `[profile.ci] inherits = "dev"` + `--profile ci` ──────
    //
    // Stock cargo: ancestor `[profile.ci] panic = "unwind"` outranks the
    // manifest's inherited `panic = "abort"` (config > manifest), so a
    // panicking test unwinds and the runner exits cleanly 1. cargo-rudzio
    // must match (no SIGABRT).

    #[rudzio::test]
    fn custom_ci_profile_inherits_dev_ancestor_unwind_matches_stock(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let parent = ctx.suite_tempdir().join("case8-ancestor");
        let root = parent.join("ws");
        fs::create_dir_all(&root)?;
        write_ancestor_config(&parent, "[profile.ci]\npanic = \"unwind\"\n")?;
        write_fixture(
            &root,
            rudzio_root()?,
            "[profile.dev]\npanic = \"abort\"\n\n[profile.ci]\ninherits = \"dev\"\n",
            None,
            "custom_ci_panic",
            "            panic!(\"boom\")\n",
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &["--profile", "ci"],
            &[],
        )?;
        let status = output.status;
        let text = combined(&output);
        anyhow::ensure!(
            status.code() == Some(1_i32),
            "expected `--profile ci` (ancestor [profile.ci] panic=unwind) to unwind to a clean \
             exit 1 matching stock cargo, but got {status:?} (code={:?}, signal={:?}).\n{text}",
            status.code(),
            status.signal(),
        );
        anyhow::ensure!(
            text.contains("panicked"),
            "expected the panicking test to be reported (unwind), but saw:\n{text}",
        );
        Ok(())
    }

    // ── 11. `--config KEY=VALUE` forwarded to the inner build ────────────

    #[rudzio::test]
    fn command_line_config_forwarded_to_inner_build(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let root = ctx.suite_tempdir().join("case11-cli-config");
        fs::create_dir_all(&root)?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            None,
            "cli_config_cfg",
            r#"            anyhow::ensure!(cfg!(cli_config_flag), "--config rustflags --cfg must be forwarded to the inner build");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[
                "--config",
                "build.rustflags=[\"--cfg\",\"cli_config_flag\",\"--check-cfg=cfg(cli_config_flag)\"]",
            ],
            &[],
        )?;
        assert_passed(&output, "cli_config_cfg")
    }

    // ── 4b. ancestor `[build] rustflags` MERGE with consumer's ───────────
    //
    // Cargo merges `[build] rustflags` across config layers: an ancestor
    // config's flags are NOT overridden by the consumer's, both apply.

    #[rudzio::test]
    fn config_build_rustflags_merge_across_ancestor(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let parent = ctx.suite_tempdir().join("case4b-ancestor");
        let root = parent.join("ws");
        fs::create_dir_all(&root)?;
        write_ancestor_config(
            &parent,
            "[build]\nrustflags = [\"--cfg\", \"ancestor_flag\"]\n",
        )?;
        write_fixture(
            &root,
            rudzio_root()?,
            "",
            Some(
                "[build]\nrustflags = [\"--cfg\", \"consumer_flag\", \"--check-cfg=cfg(consumer_flag)\"]\n",
            ),
            "rustflags_merge",
            r#"            anyhow::ensure!(cfg!(consumer_flag), "consumer [build] rustflags must apply");
            anyhow::ensure!(cfg!(ancestor_flag), "ancestor [build] rustflags must ALSO apply (cargo merges, not overrides)");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "rustflags_merge")
    }

    // ── 9. `[profile.dev.package."*"]` vs ancestor `[profile.dev]` ───────
    //
    // Stock cargo: the ancestor `.cargo/config.toml` `[profile.dev]
    // debug-assertions = true` outranks the manifest's per-package
    // `[profile.dev.package."*"] debug-assertions = false` (config > manifest,
    // even against the more specific package override). cargo-rudzio must
    // match.

    #[rudzio::test]
    fn per_package_override_ancestor_config_outranks(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let parent = ctx.suite_tempdir().join("case9-ancestor");
        let root = parent.join("ws");
        fs::create_dir_all(&root)?;
        write_ancestor_config(&parent, "[profile.dev]\ndebug-assertions = true\n")?;
        write_fixture(
            &root,
            rudzio_root()?,
            "[profile.dev.package.\"*\"]\ndebug-assertions = false\n",
            None,
            "per_package_ancestor_wins",
            r#"            anyhow::ensure!(cfg!(debug_assertions), "ancestor [profile.dev] debug-assertions=true must outrank manifest package.\"*\" false");
            Ok(())
"#,
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &[],
            &[],
        )?;
        assert_passed(&output, "per_package_ancestor_wins")
    }

    // ── 2. `[profile.dev] opt-level` via `-v` rustc args ─────────────────
    //
    // opt-level has no `cfg!` observable, so we inspect the inner build's
    // rustc args via the forwarded `-v`. Ancestor `opt-level = 0` outranks
    // the manifest `opt-level = 1` (config > manifest), same as stock cargo.

    #[rudzio::test]
    fn profile_opt_level_ancestor_outranks_via_verbose(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let parent = ctx.suite_tempdir().join("case2-ancestor");
        let root = parent.join("ws");
        fs::create_dir_all(&root)?;
        // Use non-default opt-levels on BOTH sides so `-v` prints the
        // explicit `-C opt-level=N` (opt-level 0 is the default and cargo
        // elides it from the rustc argv).
        write_ancestor_config(&parent, "[profile.dev]\nopt-level = 1\n")?;
        write_fixture(
            &root,
            rudzio_root()?,
            "[profile.dev]\nopt-level = 3\n",
            None,
            "opt_level_probe",
            "            Ok(())\n",
        )?;
        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            &cargo_build_dir_for(ctx.cargo_target_dir())?,
            &root,
            &["-v"],
            &[],
        )?;
        let text = combined(&output);
        anyhow::ensure!(
            output.status.success(),
            "expected the -v run to pass, but cargo rudzio exited {:?}.\n{text}",
            output.status.code(),
        );
        anyhow::ensure!(
            text.contains("opt-level=1"),
            "ancestor [profile.dev] opt-level=1 must reach the inner rustc args.\n{text}",
        );
        anyhow::ensure!(
            !text.contains("opt-level=3"),
            "manifest opt-level=3 must NOT reach the inner rustc args (ancestor config outranks).\n{text}",
        );
        Ok(())
    }
}
