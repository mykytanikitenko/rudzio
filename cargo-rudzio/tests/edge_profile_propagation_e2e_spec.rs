//! Edge cases for workspace `[profile]` propagation into the generated
//! aggregator (Bug #4). Cargo honours `[profile.*]` only at the workspace
//! root; the aggregator is its own root, so cargo-rudzio re-emits the user's
//! `[profile]` table verbatim into the generated manifest. This spec pins the
//! propagation contract across the profile shapes users actually rely on:
//!
//!   1. a custom `[profile.ci]` reached via `--profile ci` aborts on panic
//!      (SIGABRT), mirroring the `[profile.release]` + `--release` case;
//!   2. `[profile.dev.package."*"]` and `[profile.dev.build-override]`
//!      subtables round-trip verbatim into the generated Cargo.toml;
//!   3. `[profile.dev] debug-assertions = false` reaches the *member* code
//!      compiled through the bridge (`!cfg!(debug_assertions)` holds);
//!   4. `[profile.dev] overflow-checks = false` reaches member code
//!      (wrapping arithmetic does not panic);
//!   5. a `[profile]` table on a NON-root member is ignored (matching stock
//!      cargo) — the aggregator does not copy it;
//!   6. no `[profile]` section at all yields a generated manifest with no
//!      profile table;
//!   7. with `panic = "abort"`, a test that spawns a child process and then
//!      panics (uncaught) still aborts the runner — the SIGABRT exit
//!      propagates through cargo-rudzio's `RunnerExit::Abort` → `abort()`.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
use rudzio::runtime::tokio::Multithread as TokioMultithread;
use tempfile::TempDir;

/// Repo root = parent of `cargo-rudzio/` (where this spec lives). Used as
/// the `path` for the fixture's `rudzio` workspace dependency.
fn rudzio_source_root() -> Option<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent()
}

/// Write a workspace whose single `app` member compiles `test_body` as its
/// one `#[rudzio::test]` fn. `profile` is the raw `[profile.*]` TOML embedded
/// in the root manifest (empty to omit profiles). The test fn body is spliced
/// verbatim, so e2e cases drive distinct behaviours (panic, debug-assertions
/// check, overflow arithmetic, child-spawn-then-panic) from one template.
fn write_e2e_fixture(
    root: &Path,
    rudzio_root: &Path,
    profile: &str,
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
            "// rudzio_test marker
#[cfg(any(test, rudzio_test))]
mod tests {{
    use rudzio::common::context::{{Suite, Test}};
    use rudzio::runtime::tokio::Multithread as TokioMultithread;

    #[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]
    mod inner {{
        #[rudzio::test]
        fn profile_test(_t: &super::Test) -> anyhow::Result<()> {{
{test_body}
        }}
    }}
}}
",
        ),
    )?;

    Ok(())
}

/// Write a workspace with a marker-only `app` member (no actual test fn) so
/// `plan_from_manifest` produces a non-empty plan for unit-level assertions
/// on the generated aggregator manifest.
fn write_unit_fixture(root: &Path, rudzio_root: &Path, profile: &str) -> io::Result<()> {
    write_e2e_fixture(root, rudzio_root, profile, "            Ok(())\n")?;
    // Overwrite the member lib with a marker-only body: the unit tests only
    // assert on the generated Cargo.toml, not on a runnable test surface.
    let app = root.join("app");
    fs::write(app.join("src").join("lib.rs"), "// rudzio_test marker\n")?;
    Ok(())
}

/// Read the generated aggregator Cargo.toml for a workspace at `root`.
/// Wraps `plan_from_manifest` + `write_runner` and returns the manifest text.
fn generated_aggregator_manifest(root: &Path) -> anyhow::Result<String> {
    let manifest = root.join("Cargo.toml");
    let plan = plan_from_manifest(&manifest)?;
    let out_dir = plan.default_output_dir();
    write_runner(&plan, &out_dir)?;
    Ok(fs::read_to_string(out_dir.join("Cargo.toml"))?)
}

#[rudzio::suite([(
    runtime = TokioMultithread::new,
    suite = IsolatedCargoSuite,
    test = IsolatedCargoTest,
)])]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::{
        Command, IsolatedCargoTest, Output, Path, TempDir, env, fs, generated_aggregator_manifest,
        io, rudzio_source_root, write_e2e_fixture, write_unit_fixture,
    };

    /// Spawn `cargo rudzio test` against the fixture at `root` with the
    /// isolated cargo env, forwarding `extra_args` (e.g. `--profile ci`).
    fn run_cargo_rudzio(
        cargo_home: &Path,
        cargo_target_dir: &Path,
        root: &Path,
        extra_args: &[&str],
    ) -> io::Result<Output> {
        Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
            .arg("test")
            .args(extra_args)
            .current_dir(root)
            .env(
                "CARGO",
                env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
            )
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", cargo_target_dir)
            .env("CARGO_NET_OFFLINE", "true")
            .output()
    }

    /// Repo root of the rudzio workspace under test. `cargo-rudzio/tests`
    /// always lives directly under the workspace root, so the parent of this
    /// spec's `CARGO_MANIFEST_DIR` is that root.
    fn rudzio_root() -> anyhow::Result<&'static Path> {
        rudzio_source_root()
            .ok_or_else(|| anyhow::anyhow!("cargo-rudzio/tests must live under the workspace root"))
    }

    // ── 1. custom `[profile.ci]` reached via `--profile ci` ───────────────

    #[rudzio::test]
    fn custom_ci_profile_aborts_on_panic(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_e2e_fixture(
            root,
            rudzio_root()?,
            "[profile.ci]\ninherits = \"dev\"\npanic = \"abort\"\n",
            "            panic!(\"boom\")\n",
        )?;

        let output = run_cargo_rudzio(
            ctx.cargo_home(),
            ctx.cargo_target_dir(),
            root,
            &["--profile", "ci"],
        )?;
        let status = output.status;
        let aborted =
            status.signal() == Some(6_i32) || (!status.success() && status.code().is_none());
        anyhow::ensure!(
            aborted,
            "expected `--profile ci` (with [profile.ci] panic = \"abort\") to abort the runner, \
             but it exited {status:?} (code={:?}, signal={:?}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status.code(),
            status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }

    // ── 3. debug-assertions = false reaches member code ───────────────────

    #[rudzio::test]
    fn debug_assertions_false_reaches_member_code(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_e2e_fixture(
            root,
            rudzio_root()?,
            "[profile.dev]\ndebug-assertions = false\n",
            "            anyhow::ensure!(\n                !cfg!(debug_assertions),\n                \"debug_assertions must be OFF in the member code compiled through the bridge\",\n            );\n            Ok(())\n",
        )?;

        let output = run_cargo_rudzio(ctx.cargo_home(), ctx.cargo_target_dir(), root, &[])?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::ensure!(
            output.status.success(),
            "expected the debug-assertions-false test to pass, but it exited {:?}.\n{combined}",
            output.status.code(),
        );
        anyhow::ensure!(combined.contains("1 passed"), "output:\n{combined}");
        Ok(())
    }

    // ── 4. overflow-checks = false reaches member code ─────────────────────

    #[rudzio::test]
    fn overflow_checks_false_reaches_member_code(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_e2e_fixture(
            root,
            rudzio_root()?,
            "[profile.dev]\noverflow-checks = false\n",
            "            let v: u8 = std::hint::black_box(200);\n            let doubled: u8 = v * 2;\n            anyhow::ensure!(\n                doubled == 144,\n                \"overflow-checks=false should wrap 200*2 to 144, got {doubled}\",\n            );\n            Ok(())\n",
        )?;

        let output = run_cargo_rudzio(ctx.cargo_home(), ctx.cargo_target_dir(), root, &[])?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::ensure!(
            output.status.success(),
            "expected the overflow-checks-false test to pass (wraparound, no panic), but it \
             exited {:?}.\n{combined}",
            output.status.code(),
        );
        anyhow::ensure!(combined.contains("1 passed"), "output:\n{combined}");
        Ok(())
    }

    // ── 7. panic=abort with a spawned child still propagates SIGABRT ───────

    #[rudzio::test]
    fn panic_abort_with_spawned_child_propagates_sigabrt(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let root: &Path = workspace.path();
        write_e2e_fixture(
            root,
            rudzio_root()?,
            "[profile.dev]\npanic = \"abort\"\n",
            "            // Spawn a short-lived child process, then panic uncaught.\n            let _ = std::process::Command::new(\"true\").status();\n            panic!(\"boom-after-spawn\")\n",
        )?;

        let output = run_cargo_rudzio(ctx.cargo_home(), ctx.cargo_target_dir(), root, &[])?;
        let status = output.status;
        let aborted =
            status.signal() == Some(6_i32) || (!status.success() && status.code().is_none());
        anyhow::ensure!(
            aborted,
            "expected the runner to abort (SIGABRT) even after spawning a child, but it exited \
             {status:?} (code={:?}, signal={:?}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status.code(),
            status.signal(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }

    // ── 2. package."*" + build-override subtables round-trip verbatim ──────

    #[rudzio::test]
    fn package_override_and_build_override_round_trip(
        _ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        write_unit_fixture(
            root,
            rudzio_root()?,
            "[profile.dev.package.\"*\"]\nopt-level = 1\n\n[profile.dev.build-override]\nopt-level = 1\n",
        )?;

        let agg = generated_aggregator_manifest(root)?;
        anyhow::ensure!(
            agg.contains("[profile.dev.package.\"*\"]"),
            "generated manifest must preserve the package.\"*\" subtable:\n{agg}",
        );
        anyhow::ensure!(
            agg.contains("opt-level = 1"),
            "generated manifest must preserve opt-level = 1:\n{agg}",
        );
        anyhow::ensure!(
            agg.contains("[profile.dev.build-override]"),
            "generated manifest must preserve the build-override subtable:\n{agg}",
        );
        Ok(())
    }

    // ── 5. profile on a NON-root member is not copied ──────────────────────

    #[rudzio::test]
    fn member_level_profile_is_not_copied(_ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        // Root manifest carries no `[profile]`; the member declares one.
        write_unit_fixture(root, rudzio_root()?, "")?;
        let app = root.join("app");
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

[profile.dev]
panic = "abort"

[dev-dependencies]
rudzio = { workspace = true }
anyhow = { workspace = true }
"#,
        )?;

        let agg = generated_aggregator_manifest(root)?;
        anyhow::ensure!(
            !agg.contains("[profile"),
            "member-level [profile] must NOT be copied into the aggregator (cargo ignores it \
             there); got:\n{agg}",
        );
        Ok(())
    }

    // ── 6. no [profile] section yields no profile table ────────────────────

    #[rudzio::test]
    fn no_profile_section_yields_no_profile_table(_ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        write_unit_fixture(root, rudzio_root()?, "")?;

        let agg = generated_aggregator_manifest(root)?;
        anyhow::ensure!(
            !agg.contains("[profile"),
            "generated manifest must omit [profile] when the root declares none:\n{agg}",
        );
        Ok(())
    }
}
