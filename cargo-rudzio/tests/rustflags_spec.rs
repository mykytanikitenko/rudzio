use cargo_rudzio::resolve_rustflags;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::resolve_rustflags;

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
}
