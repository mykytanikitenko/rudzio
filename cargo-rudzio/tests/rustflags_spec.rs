use cargo_rudzio::{EXPOSE_BINS_SENTINEL_ENV, resolve_rustflags, spawn_env};

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::{EXPOSE_BINS_SENTINEL_ENV, resolve_rustflags, spawn_env};

    #[rudzio::test]
    fn injects_rudzio_test_cfg_when_rustflags_unset() -> anyhow::Result<()> {
        anyhow::ensure!(resolve_rustflags(None) == "--cfg rudzio_test");
        Ok(())
    }

    #[rudzio::test]
    fn appends_to_existing_rustflags_preserving_them() -> anyhow::Result<()> {
        anyhow::ensure!(
            resolve_rustflags(Some("-C opt-level=1")) == "-C opt-level=1 --cfg rudzio_test",
        );
        Ok(())
    }

    #[rudzio::test]
    fn idempotent_when_flag_already_present() -> anyhow::Result<()> {
        anyhow::ensure!(resolve_rustflags(Some("--cfg rudzio_test")) == "--cfg rudzio_test");
        anyhow::ensure!(
            resolve_rustflags(Some("-C opt-level=1 --cfg rudzio_test"))
                == "-C opt-level=1 --cfg rudzio_test",
        );
        anyhow::ensure!(
            resolve_rustflags(Some("--cfg rudzio_test -C opt-level=1"))
                == "--cfg rudzio_test -C opt-level=1",
        );
        Ok(())
    }

    #[rudzio::test]
    fn treats_blank_rustflags_as_unset() -> anyhow::Result<()> {
        anyhow::ensure!(resolve_rustflags(Some("")) == "--cfg rudzio_test");
        anyhow::ensure!(resolve_rustflags(Some("   ")) == "--cfg rudzio_test");
        anyhow::ensure!(resolve_rustflags(Some("\t\n")) == "--cfg rudzio_test");
        Ok(())
    }

    #[rudzio::test]
    fn does_not_collide_with_other_cfg_flags() -> anyhow::Result<()> {
        anyhow::ensure!(
            resolve_rustflags(Some("--cfg foo --cfg bar"))
                == "--cfg foo --cfg bar --cfg rudzio_test",
        );
        Ok(())
    }

    #[rudzio::test]
    fn spawn_env_sets_expose_bins_sentinel() -> anyhow::Result<()> {
        // Reproduction: without this env var set, a bridge crate forwarding
        // a member's `build.rs` that calls `rudzio::build::expose_self_bins()`
        // errors with "package `<bridge>_rudzio_bridge` declares no `[[bin]]`
        // targets". The sentinel makes expose_bins detect re-entry and
        // early-return Ok.
        let env = spawn_env(None);
        let sentinel = env
            .iter()
            .find(|(k, _)| *k == EXPOSE_BINS_SENTINEL_ENV)
            .map(|(_, v)| v.as_str());
        anyhow::ensure!(
            sentinel == Some("1"),
            "spawn_env must set the expose-bins re-entry sentinel to \"1\", got {sentinel:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn spawn_env_preserves_rustflags_semantics() -> anyhow::Result<()> {
        let env = spawn_env(Some("-C opt-level=2"));
        let rustflags = env
            .iter()
            .find(|(k, _)| *k == "RUSTFLAGS")
            .map(|(_, v)| v.as_str());
        anyhow::ensure!(
            rustflags == Some("-C opt-level=2 --cfg rudzio_test"),
            "spawn_env must delegate to resolve_rustflags, got {rustflags:?}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn expose_bins_sentinel_name_matches_rudzio_build_module() -> anyhow::Result<()> {
        // Contract: this name MUST match the const `NESTED_SENTINEL_ENV`
        // in `rudzio/src/build.rs`. If either changes without the other,
        // the sentinel stops working and bridge-forwarded build.rs calls
        // to expose_self_bins regress to "no [[bin]]" errors.
        anyhow::ensure!(
            EXPOSE_BINS_SENTINEL_ENV == "__RUDZIO_EXPOSE_BINS_ACTIVE",
            "sentinel env-var name drift detected: {EXPOSE_BINS_SENTINEL_ENV}"
        );
        Ok(())
    }
}
