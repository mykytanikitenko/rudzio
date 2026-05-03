//! TDD spec for `cargo_rudzio::args::parse_package_filters`.
//!
//! `cargo rudzio test -p <NAME>` must (a) consume the flag pair so it
//! never reaches the rudzio runner — where `-p` would be flagged as
//! unrecognised and `<NAME>` would silently land as a positional
//! substring filter that almost never matches a fully-qualified test —
//! and (b) preserve every other arg in original order so downstream
//! splitters (path-vs-runner, runner-binary forwarder) see the exact
//! same shape they would have without `-p` present.
//!
//! All four cargo spellings are accepted: short/long × space/equals.

use cargo_rudzio::args::parse_package_filters;
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::parse_package_filters;

    /// Convenience: build a `Vec<String>` from a slice of `&str` in
    /// one expression so the test bodies stay readable.
    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[rudzio::test]
    async fn no_package_flag_returns_empty_packages_and_unchanged_args() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let (packages, remaining) = parse_package_filters(&input)?;
        anyhow::ensure!(packages.is_empty(), "expected no packages, got {packages:?}");
        anyhow::ensure!(
            remaining == input,
            "expected args untouched, got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn dash_p_space_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["-p", "rudzio-migrate", "my_filter"]);
        let (packages, remaining) = parse_package_filters(&input)?;
        anyhow::ensure!(
            packages == vec!["rudzio-migrate".to_owned()],
            "got {packages:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn dash_p_equals_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["-p=rudzio-migrate", "my_filter"]);
        let (packages, remaining) = parse_package_filters(&input)?;
        anyhow::ensure!(
            packages == vec!["rudzio-migrate".to_owned()],
            "got {packages:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn long_package_space_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--package", "rudzio-migrate", "my_filter"]);
        let (packages, remaining) = parse_package_filters(&input)?;
        anyhow::ensure!(
            packages == vec!["rudzio-migrate".to_owned()],
            "got {packages:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn long_package_equals_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--package=rudzio-migrate", "my_filter"]);
        let (packages, remaining) = parse_package_filters(&input)?;
        anyhow::ensure!(
            packages == vec!["rudzio-migrate".to_owned()],
            "got {packages:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn repeated_package_flags_collect_in_order() -> anyhow::Result<()> {
        let input = argv(&[
            "-p",
            "first",
            "--skip",
            "noise",
            "--package=second",
            "my_filter",
        ]);
        let (packages, remaining) = parse_package_filters(&input)?;
        anyhow::ensure!(
            packages == vec!["first".to_owned(), "second".to_owned()],
            "got {packages:?}",
        );
        anyhow::ensure!(
            remaining == argv(&["--skip", "noise", "my_filter"]),
            "got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn dash_p_without_value_errors() -> anyhow::Result<()> {
        let input = argv(&["-p"]);
        let Err(err) = parse_package_filters(&input) else {
            anyhow::bail!("expected error for trailing -p with no value");
        };
        anyhow::ensure!(
            err.to_string().contains("requires a package name"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn long_package_without_value_errors() -> anyhow::Result<()> {
        let input = argv(&["--package"]);
        let Err(err) = parse_package_filters(&input) else {
            anyhow::bail!("expected error for trailing --package with no value");
        };
        anyhow::ensure!(
            err.to_string().contains("requires a package name"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn empty_equals_form_errors() -> anyhow::Result<()> {
        for empty in [argv(&["-p="]), argv(&["--package="])] {
            let Err(err) = parse_package_filters(&empty) else {
                anyhow::bail!("expected error for empty value form: {empty:?}");
            };
            anyhow::ensure!(
                err.to_string().contains("non-empty package name"),
                "got {err} for {empty:?}",
            );
        }
        Ok(())
    }
}
