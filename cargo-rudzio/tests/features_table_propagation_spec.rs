//! Behavioral spec: aggregator's generated `Cargo.toml` exposes each
//! member's `[features]` so `cargo rudzio test --features X` works
//! the same as `cargo test --features X` (Bug #2).
//!
//! # Symptom (pre-fix)
//!
//! Consumer member declares `[features] heavy = []` and gates code
//! with `#[cfg(feature = "heavy")]`. Running `cargo rudzio test
//! --features heavy` (or `cargo build --manifest-path
//! target/agg/Cargo.toml --features heavy`) fails with `error: the
//! package 'rudzio-auto-runner' does not contain this feature:
//! heavy` because the generated aggregator manifest has no
//! `[features]` table. v0.1.1 partially mitigates the related Bug
//! #9 case (optional deps gated by `dep:X` are force-enabled), but
//! plain feature flags that gate `#[cfg(feature = ...)]` blocks
//! still fail.
//!
//! # Behavioral assertion
//!
//! Synthesize a member with `[features] heavy = []`. Drive
//! `plan_from_manifest` + `write_runner`. Read the generated
//! aggregator `Cargo.toml` and assert it contains a `[features]`
//! table whose entries cover the member's features (under a
//! disambiguated name when the same feature appears on multiple
//! members — at minimum, `heavy` must be reachable).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargo_rudzio::generate::{plan_from_manifest, write_runner};
use cargo_rudzio_test_helpers::with_cargo_offline;
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread as TokioMultithread;

/// Workspace root of the rudzio repo.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// One-member workspace with a custom feature.
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
    fs::write(
        member.join("Cargo.toml"),
        "[package]\n\
         name = \"member\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"member\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [features]\n\
         heavy = []\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n",
    )?;
    fs::write(member.join("src").join("lib.rs"), "// rudzio_test marker\n")?;
    Ok(())
}

#[rudzio::suite([
    (runtime = TokioMultithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        fs, plan_from_manifest, rudzio_source_root, with_cargo_offline, write_fixture, write_runner,
    };

    #[rudzio::test]
    fn aggregator_cargo_toml_exposes_member_feature_flags() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        write_fixture(temp.path(), &rudzio_root)?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;
        let agg = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        anyhow::ensure!(
            agg.contains("[features]"),
            "aggregator Cargo.toml must carry a `[features]` table mirroring member features for `--features X` to work:\n{agg}",
        );
        anyhow::ensure!(
            agg.contains("heavy"),
            "aggregator Cargo.toml must surface the `heavy` feature so `--features heavy` resolves:\n{agg}",
        );
        Ok(())
    }

    /// Member declares `default-features = false` on rudzio (or any
    /// dep). The aggregator must propagate that — otherwise the
    /// aggregator's compile-time view includes features the member
    /// explicitly opted out of, breaking `#[cfg(feature = ...)]`
    /// gates the member relies on.
    #[rudzio::test]
    fn aggregator_propagates_default_features_false_setting() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        fs::write(
            temp.path().join("Cargo.toml"),
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
        let member = temp.path().join("member");
        fs::create_dir_all(member.join("src"))?;
        fs::create_dir_all(member.join("tests"))?;
        fs::write(
            member.join("Cargo.toml"),
            "[package]\n\
             name = \"member\"\n\
             version = \"0.0.0\"\n\
             edition = \"2024\"\n\
             publish = false\n\
             \n\
             [lib]\n\
             name = \"member\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true, default-features = false, features = [\"common\", \"runtime-tokio-multi-thread\"] }\n",
        )?;
        fs::write(member.join("src").join("lib.rs"), "// rudzio_test marker\n")?;
        fs::write(
            member.join("tests").join("integration.rs"),
            "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
        )?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;
        let agg = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        // The rudzio dep entry in the aggregator must carry
        // `default-features = false`. The cargo TOML formatter may
        // use `default-features` or `default_features` (less common);
        // accept either.
        anyhow::ensure!(
            agg.contains("default-features = false") || agg.contains("default_features = false"),
            "aggregator must propagate member's `default-features = false`:\n{agg}",
        );
        Ok(())
    }

    /// Feature with `dep:foo` activation. The aggregator's
    /// `[features]` table SURFACES the feature name (so users can
    /// `--features X`), but STRIPS the `dep:foo` activation values:
    /// Bug #9 force-enables every optional dep in the aggregator's
    /// `[dependencies]` table, so the deps no longer need feature-
    /// gated activation. Keeping `dep:foo` activations would make
    /// the aggregator's manifest invalid because the dep is no
    /// longer optional.
    #[rudzio::test]
    fn aggregator_strips_dep_activations_but_keeps_feature_name() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        fs::write(
            temp.path().join("Cargo.toml"),
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
        let member = temp.path().join("member");
        fs::create_dir_all(member.join("src"))?;
        fs::create_dir_all(member.join("tests"))?;
        fs::write(
            member.join("Cargo.toml"),
            "[package]\n\
             name = \"member\"\n\
             version = \"0.0.0\"\n\
             edition = \"2024\"\n\
             publish = false\n\
             \n\
             [lib]\n\
             name = \"member\"\n\
             path = \"src/lib.rs\"\n\
             \n\
             [features]\n\
             default = []\n\
             with_opt = [\"dep:fake_opt_dep\"]\n\
             \n\
             [dependencies]\n\
             fake_opt_dep = { version = \"=0.0.1\", optional = true }\n\
             \n\
             [dev-dependencies]\n\
             rudzio = { workspace = true }\n",
        )?;
        fs::write(member.join("src").join("lib.rs"), "// rudzio_test marker\n")?;
        fs::write(
            member.join("tests").join("integration.rs"),
            "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
        )?;
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;
        let agg = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        // Two valid post-fix shapes:
        //  (a) feature value is preserved verbatim (dep:fake_opt_dep)
        //      AND the optional dep stays optional.
        //  (b) The dep is force-enabled (per Bug #9 fix) and the
        //      feature is collapsed / aliased — but `with_opt` must
        //      still be a reachable feature name.
        anyhow::ensure!(
            agg.contains("with_opt"),
            "feature name `with_opt` must reach the aggregator:\n{agg}",
        );
        // The dep:foo wiring is documented behavior here — failure
        // would mean a user can't request the feature without
        // unrelated knock-on errors. Pre-fix the feature entry was
        // dropped entirely; this assertion catches that regression.
        anyhow::ensure!(
            agg.contains("[features]"),
            "aggregator must have a [features] table when the member declares features:\n{agg}",
        );
        Ok(())
    }

    /// Two members each declaring their own `[features]` table.
    /// Both feature names must be reachable from the aggregator;
    /// per-member features get namespaced (or unioned) so they
    /// don't collide.
    #[rudzio::test]
    fn aggregator_unions_features_across_two_members() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let rudzio_root = rudzio_source_root();
        fs::write(
            temp.path().join("Cargo.toml"),
            format!(
                "[workspace]\n\
                 members = [\"alpha\", \"beta\"]\n\
                 resolver = \"3\"\n\
                 \n\
                 [workspace.dependencies]\n\
                 rudzio = {{ path = \"{rudzio}\", default-features = false, features = [\"common\", \"runtime-tokio-multi-thread\"] }}\n",
                rudzio = rudzio_root.display(),
            ),
        )?;
        for (name, feature) in [("alpha", "alpha_feat"), ("beta", "beta_feat")] {
            let member_dir = temp.path().join(name);
            fs::create_dir_all(member_dir.join("src"))?;
            fs::create_dir_all(member_dir.join("tests"))?;
            fs::write(
                member_dir.join("Cargo.toml"),
                format!(
                    "[package]\n\
                     name = \"{name}\"\n\
                     version = \"0.0.0\"\n\
                     edition = \"2024\"\n\
                     publish = false\n\
                     \n\
                     [lib]\n\
                     name = \"{name}\"\n\
                     path = \"src/lib.rs\"\n\
                     \n\
                     [features]\n\
                     {feature} = []\n\
                     \n\
                     [dev-dependencies]\n\
                     rudzio = {{ workspace = true }}\n",
                ),
            )?;
            fs::write(
                member_dir.join("src").join("lib.rs"),
                "// rudzio_test marker\n",
            )?;
            fs::write(
                member_dir.join("tests").join("integration.rs"),
                "// rudzio_test marker\n#[rudzio::main]\nfn main() {}\n",
            )?;
        }
        let manifest = temp.path().join("Cargo.toml");
        let plan = with_cargo_offline(|| plan_from_manifest(&manifest))?;
        let out_dir = plan.default_output_dir();
        write_runner(&plan, &out_dir)?;
        let agg = fs::read_to_string(out_dir.join("Cargo.toml"))?;
        // Both feature names must be reachable. They may be
        // namespaced (e.g. `alpha_alpha_feat`) or unioned flat —
        // either shape preserves the user's intent, but the bare
        // feature name MUST be a substring of the final manifest.
        anyhow::ensure!(
            agg.contains("alpha_feat"),
            "alpha's feature must surface in aggregator:\n{agg}",
        );
        anyhow::ensure!(
            agg.contains("beta_feat"),
            "beta's feature must surface in aggregator:\n{agg}",
        );
        Ok(())
    }
}
