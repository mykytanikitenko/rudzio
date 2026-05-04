//! TDD spec for the cargo-style argument parsers in `cargo_rudzio::cli`.
//!
//! Each parser must (a) consume the flag pair(s) it owns so they never
//! reach the rudzio runner — where unknown flags would be warned about
//! and the values would silently land as positional substring filters —
//! and (b) preserve every other arg in original order so downstream
//! splitters (path-vs-runner, runner-binary forwarder) see the exact
//! same shape they would have without the consumed flags present.
//!
//! All four cargo spellings are accepted where applicable: short/long
//! × space/equals.

use std::path::Path;

use cargo_rudzio::cli::{parse_exclude_filters, parse_package_filters, parse_test_args};
use rudzio::common::context::Suite;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{Path, parse_exclude_filters, parse_package_filters, parse_test_args};

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

    #[rudzio::test]
    async fn no_exclude_flag_returns_empty_excludes_and_unchanged_args() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let (excluded, remaining) = parse_exclude_filters(&input)?;
        anyhow::ensure!(excluded.is_empty(), "expected no excludes, got {excluded:?}");
        anyhow::ensure!(
            remaining == input,
            "expected args untouched, got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn exclude_space_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--exclude", "rudzio-migrate", "my_filter"]);
        let (excluded, remaining) = parse_exclude_filters(&input)?;
        anyhow::ensure!(
            excluded == vec!["rudzio-migrate".to_owned()],
            "got {excluded:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn exclude_equals_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--exclude=rudzio-migrate", "my_filter"]);
        let (excluded, remaining) = parse_exclude_filters(&input)?;
        anyhow::ensure!(
            excluded == vec!["rudzio-migrate".to_owned()],
            "got {excluded:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn repeated_exclude_flags_collect_in_order() -> anyhow::Result<()> {
        let input = argv(&[
            "--exclude",
            "first",
            "--skip",
            "noise",
            "--exclude=second",
            "my_filter",
        ]);
        let (excluded, remaining) = parse_exclude_filters(&input)?;
        anyhow::ensure!(
            excluded == vec!["first".to_owned(), "second".to_owned()],
            "got {excluded:?}",
        );
        anyhow::ensure!(
            remaining == argv(&["--skip", "noise", "my_filter"]),
            "got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn exclude_without_value_errors() -> anyhow::Result<()> {
        let input = argv(&["--exclude"]);
        let Err(err) = parse_exclude_filters(&input) else {
            anyhow::bail!("expected error for trailing --exclude with no value");
        };
        anyhow::ensure!(
            err.to_string().contains("requires a package name"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn exclude_empty_equals_form_errors() -> anyhow::Result<()> {
        let input = argv(&["--exclude="]);
        let Err(err) = parse_exclude_filters(&input) else {
            anyhow::bail!("expected error for empty --exclude= value");
        };
        anyhow::ensure!(
            err.to_string().contains("non-empty package name"),
            "got {err}",
        );
        Ok(())
    }

    /// Stub `is_dir` predicate that treats nothing on disk as a directory.
    /// Use when the test asserts that path-shaped args without filesystem
    /// existence flow through to the runner side.
    const fn no_dirs(_path: &Path) -> bool {
        false
    }

    #[rudzio::test]
    async fn parse_test_args_empty_input_yields_empty_filters_and_runner() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&[]), no_dirs)?;
        anyhow::ensure!(parsed.filters.include_packages.is_empty());
        anyhow::ensure!(parsed.filters.exclude_packages.is_empty());
        anyhow::ensure!(parsed.filters.include_paths.is_empty());
        anyhow::ensure!(parsed.runner_args.is_empty());
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_collects_packages_into_filters() -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["-p", "rudzio", "--package=rudzio-migrate"]),
            no_dirs,
        )?;
        anyhow::ensure!(
            parsed.filters.include_packages == vec!["rudzio".to_owned(), "rudzio-migrate".to_owned()],
            "got {:?}",
            parsed.filters.include_packages,
        );
        anyhow::ensure!(parsed.runner_args.is_empty(), "runner_args should be empty");
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_collects_excludes_into_filters() -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["--exclude", "noisy", "--exclude=other"]),
            no_dirs,
        )?;
        anyhow::ensure!(
            parsed.filters.exclude_packages == vec!["noisy".to_owned(), "other".to_owned()],
            "got {:?}",
            parsed.filters.exclude_packages,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_routes_path_shaped_existing_dirs_to_filters() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["./crates/foo", "my_filter"]), |path| {
            path == Path::new("./crates/foo")
        })?;
        anyhow::ensure!(
            parsed.filters.include_paths == vec![Path::new("./crates/foo").to_path_buf()],
            "got {:?}",
            parsed.filters.include_paths,
        );
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_path_shaped_missing_dir_falls_through_to_runner() -> anyhow::Result<()>
    {
        let parsed = parse_test_args(&argv(&["./typoed-path"]), no_dirs)?;
        anyhow::ensure!(
            parsed.filters.include_paths.is_empty(),
            "missing path should not become a filter",
        );
        anyhow::ensure!(
            parsed.runner_args == vec!["./typoed-path".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_runner_args_preserve_original_order() -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["my_filter", "--skip", "slow_", "--output=plain"]),
            no_dirs,
        )?;
        anyhow::ensure!(
            parsed.runner_args
                == vec![
                    "my_filter".to_owned(),
                    "--skip".to_owned(),
                    "slow_".to_owned(),
                    "--output=plain".to_owned(),
                ],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_combines_packages_excludes_paths_and_runner_in_one_pass()
    -> anyhow::Result<()> {
        let input = argv(&[
            "-p",
            "kept",
            "--exclude",
            "dropped",
            "./real-dir",
            "--skip",
            "slow_",
            "my_filter",
        ]);
        let parsed = parse_test_args(&input, |path| path == Path::new("./real-dir"))?;
        anyhow::ensure!(
            parsed.filters.include_packages == vec!["kept".to_owned()],
            "got {:?}",
            parsed.filters.include_packages,
        );
        anyhow::ensure!(
            parsed.filters.exclude_packages == vec!["dropped".to_owned()],
            "got {:?}",
            parsed.filters.exclude_packages,
        );
        anyhow::ensure!(
            parsed.filters.include_paths == vec![Path::new("./real-dir").to_path_buf()],
            "got {:?}",
            parsed.filters.include_paths,
        );
        anyhow::ensure!(
            parsed.runner_args == vec!["--skip".to_owned(), "slow_".to_owned(), "my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_bubbles_parser_errors() -> anyhow::Result<()> {
        let input = argv(&["-p"]);
        let Err(err) = parse_test_args(&input, no_dirs) else {
            anyhow::bail!("expected error for trailing -p");
        };
        anyhow::ensure!(
            err.to_string().contains("requires a package name"),
            "got {err}",
        );
        Ok(())
    }
}
