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

use cargo_rudzio::cli::{
    aggregator_cargo_args, format_target_flag_warning, parse_build_forwarder_flags,
    parse_capture_flags, parse_exclude_filters, parse_manifest_path_flag,
    parse_no_fail_fast_flag, parse_no_run_flag, parse_package_filters,
    parse_target_selection_flags, parse_test_args, parse_workspace_flag,
};
use rudzio::common::context::Suite;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{compio, embassy};

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        Path, aggregator_cargo_args, format_target_flag_warning, parse_build_forwarder_flags,
        parse_capture_flags, parse_exclude_filters, parse_manifest_path_flag,
        parse_no_fail_fast_flag, parse_no_run_flag, parse_package_filters,
        parse_target_selection_flags, parse_test_args, parse_workspace_flag,
    };

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

    #[rudzio::test]
    async fn no_run_flag_absent_returns_false_and_unchanged_args() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let (no_run, remaining) = parse_no_run_flag(&input);
        anyhow::ensure!(!no_run, "expected no_run = false");
        anyhow::ensure!(remaining == input, "expected args untouched, got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn no_run_flag_is_consumed_when_present() -> anyhow::Result<()> {
        let input = argv(&["--no-run"]);
        let (no_run, remaining) = parse_no_run_flag(&input);
        anyhow::ensure!(no_run, "expected no_run = true");
        anyhow::ensure!(remaining.is_empty(), "expected empty remaining, got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn no_run_flag_consumed_among_other_args_preserves_order() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--no-run", "--skip", "slow_"]);
        let (no_run, remaining) = parse_no_run_flag(&input);
        anyhow::ensure!(no_run, "expected no_run = true");
        anyhow::ensure!(
            remaining == argv(&["my_filter", "--skip", "slow_"]),
            "got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn no_run_flag_repeated_still_means_true() -> anyhow::Result<()> {
        let input = argv(&["--no-run", "--no-run"]);
        let (no_run, remaining) = parse_no_run_flag(&input);
        anyhow::ensure!(no_run, "expected no_run = true");
        anyhow::ensure!(remaining.is_empty(), "expected both consumed, got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_no_run_default_false() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&[]), no_dirs)?;
        anyhow::ensure!(!parsed.no_run, "expected default false");
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_no_run_extracted_into_struct() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--no-run", "my_filter"]), no_dirs)?;
        anyhow::ensure!(parsed.no_run, "expected no_run = true");
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_default_uses_run_subcommand() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["my_filter"]), no_dirs)?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        anyhow::ensure!(
            invocation
                == vec![
                    "run".to_owned(),
                    "--manifest-path".to_owned(),
                    "/tmp/Cargo.toml".to_owned(),
                    "--".to_owned(),
                    "my_filter".to_owned(),
                ],
            "got {invocation:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_no_run_swaps_to_build_subcommand() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--no-run"]), no_dirs)?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        anyhow::ensure!(
            invocation
                == vec![
                    "build".to_owned(),
                    "--manifest-path".to_owned(),
                    "/tmp/Cargo.toml".to_owned(),
                    "--message-format=json-render-diagnostics".to_owned(),
                ],
            "got {invocation:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_no_run_drops_runner_args() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--no-run", "would_be_filter"]), no_dirs)?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        anyhow::ensure!(
            !invocation.contains(&"--".to_owned()),
            "build path must not include `--` separator, got {invocation:?}",
        );
        anyhow::ensure!(
            !invocation.contains(&"would_be_filter".to_owned()),
            "build path must not forward runner args, got {invocation:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn workspace_flag_absent_returns_args_unchanged() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let remaining = parse_workspace_flag(&input);
        anyhow::ensure!(remaining == input, "expected args untouched, got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn workspace_long_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--workspace", "my_filter"]);
        let remaining = parse_workspace_flag(&input);
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn all_alias_for_workspace_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--all", "my_filter"]);
        let remaining = parse_workspace_flag(&input);
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn workspace_and_all_together_both_consumed_order_preserved() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--workspace", "--skip", "slow_", "--all"]);
        let remaining = parse_workspace_flag(&input);
        anyhow::ensure!(
            remaining == argv(&["my_filter", "--skip", "slow_"]),
            "got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn workspace_repeated_still_drops_all_occurrences() -> anyhow::Result<()> {
        let input = argv(&["--workspace", "--workspace", "my_filter"]);
        let remaining = parse_workspace_flag(&input);
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn capture_flags_absent_returns_args_unchanged() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let remaining = parse_capture_flags(&input);
        anyhow::ensure!(remaining == input, "expected args untouched, got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn nocapture_long_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--nocapture", "my_filter"]);
        let remaining = parse_capture_flags(&input);
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn show_output_long_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--show-output", "my_filter"]);
        let remaining = parse_capture_flags(&input);
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn nocapture_and_show_output_together_both_consumed() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--nocapture", "--skip", "slow_", "--show-output"]);
        let remaining = parse_capture_flags(&input);
        anyhow::ensure!(
            remaining == argv(&["my_filter", "--skip", "slow_"]),
            "got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_consumes_workspace_so_runner_never_sees_it() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--workspace", "my_filter"]), no_dirs)?;
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn no_fail_fast_absent_returns_args_unchanged() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let remaining = parse_no_fail_fast_flag(&input);
        anyhow::ensure!(remaining == input, "expected args untouched, got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn no_fail_fast_long_form_is_consumed() -> anyhow::Result<()> {
        let input = argv(&["--no-fail-fast", "my_filter"]);
        let remaining = parse_no_fail_fast_flag(&input);
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn no_fail_fast_repeated_drops_all_occurrences() -> anyhow::Result<()> {
        let input = argv(&["--no-fail-fast", "my_filter", "--no-fail-fast"]);
        let remaining = parse_no_fail_fast_flag(&input);
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_consumes_no_fail_fast_so_runner_never_sees_it() -> anyhow::Result<()>
    {
        let parsed = parse_test_args(&argv(&["--no-fail-fast", "my_filter"]), no_dirs)?;
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_consumes_all_so_runner_never_sees_it() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--all", "my_filter"]), no_dirs)?;
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_consumes_nocapture_so_runner_never_sees_it() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--nocapture", "my_filter"]), no_dirs)?;
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_consumes_show_output_so_runner_never_sees_it() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--show-output", "my_filter"]), no_dirs)?;
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_silent_consumers_dropped_alongside_real_filters() -> anyhow::Result<()>
    {
        let input = argv(&[
            "--workspace",
            "--all",
            "--nocapture",
            "--show-output",
            "my_filter",
            "--skip",
            "slow_",
        ]);
        let parsed = parse_test_args(&input, no_dirs)?;
        anyhow::ensure!(
            parsed.runner_args
                == vec!["my_filter".to_owned(), "--skip".to_owned(), "slow_".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn no_target_flags_returns_empty_consumed_and_unchanged_args() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let (consumed, remaining) = parse_target_selection_flags(&input)?;
        anyhow::ensure!(consumed.is_empty(), "got {consumed:?}");
        anyhow::ensure!(remaining == input, "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn lib_target_flag_consumed_and_recorded() -> anyhow::Result<()> {
        let input = argv(&["--lib", "my_filter"]);
        let (consumed, remaining) = parse_target_selection_flags(&input)?;
        anyhow::ensure!(consumed == vec!["--lib".to_owned()], "got {consumed:?}");
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn bins_examples_tests_benches_doc_all_targets_consumed() -> anyhow::Result<()> {
        let input = argv(&[
            "--bins",
            "--examples",
            "--tests",
            "--benches",
            "--all-targets",
            "--doc",
            "my_filter",
        ]);
        let (consumed, remaining) = parse_target_selection_flags(&input)?;
        anyhow::ensure!(
            consumed
                == vec![
                    "--bins".to_owned(),
                    "--examples".to_owned(),
                    "--tests".to_owned(),
                    "--benches".to_owned(),
                    "--all-targets".to_owned(),
                    "--doc".to_owned(),
                ],
            "got {consumed:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn bin_space_form_consumes_flag_and_value() -> anyhow::Result<()> {
        let input = argv(&["--bin", "rudzio-cli", "my_filter"]);
        let (consumed, remaining) = parse_target_selection_flags(&input)?;
        anyhow::ensure!(
            consumed == vec!["--bin rudzio-cli".to_owned()],
            "got {consumed:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn bin_equals_form_consumes_single_arg() -> anyhow::Result<()> {
        let input = argv(&["--bin=rudzio-cli", "my_filter"]);
        let (consumed, remaining) = parse_target_selection_flags(&input)?;
        anyhow::ensure!(
            consumed == vec!["--bin=rudzio-cli".to_owned()],
            "got {consumed:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn example_test_bench_value_forms_consumed() -> anyhow::Result<()> {
        let input = argv(&[
            "--example",
            "demo",
            "--test=integration",
            "--bench",
            "perf",
        ]);
        let (consumed, remaining) = parse_target_selection_flags(&input)?;
        anyhow::ensure!(
            consumed
                == vec![
                    "--example demo".to_owned(),
                    "--test=integration".to_owned(),
                    "--bench perf".to_owned(),
                ],
            "got {consumed:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn target_flags_mixed_with_runner_args_preserve_order() -> anyhow::Result<()> {
        let input = argv(&[
            "my_filter",
            "--lib",
            "--skip",
            "slow_",
            "--bin",
            "demo",
            "--output=plain",
        ]);
        let (consumed, remaining) = parse_target_selection_flags(&input)?;
        anyhow::ensure!(
            consumed == vec!["--lib".to_owned(), "--bin demo".to_owned()],
            "got {consumed:?}",
        );
        anyhow::ensure!(
            remaining
                == argv(&["my_filter", "--skip", "slow_", "--output=plain"]),
            "got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn bin_without_value_errors() -> anyhow::Result<()> {
        let input = argv(&["--bin"]);
        let Err(err) = parse_target_selection_flags(&input) else {
            anyhow::bail!("expected error for trailing --bin");
        };
        anyhow::ensure!(
            err.to_string().contains("requires a target name"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn example_equals_empty_errors() -> anyhow::Result<()> {
        let input = argv(&["--example="]);
        let Err(err) = parse_target_selection_flags(&input) else {
            anyhow::bail!("expected error for empty --example=");
        };
        anyhow::ensure!(
            err.to_string().contains("non-empty target name"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_records_target_flags_in_struct() -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["--lib", "--bin", "demo", "my_filter"]),
            no_dirs,
        )?;
        anyhow::ensure!(
            parsed.ignored_target_flags
                == vec!["--lib".to_owned(), "--bin demo".to_owned()],
            "got {:?}",
            parsed.ignored_target_flags,
        );
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_target_flags_default_empty() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&[]), no_dirs)?;
        anyhow::ensure!(
            parsed.ignored_target_flags.is_empty(),
            "got {:?}",
            parsed.ignored_target_flags,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn format_target_flag_warning_returns_none_when_empty() -> anyhow::Result<()> {
        anyhow::ensure!(
            format_target_flag_warning(&[]).is_none(),
            "expected None for empty input",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn format_target_flag_warning_lists_consumed_flags() -> anyhow::Result<()> {
        let warning = format_target_flag_warning(&[
            "--lib".to_owned(),
            "--bin demo".to_owned(),
        ])
        .ok_or_else(|| anyhow::anyhow!("expected Some(_) for non-empty input"))?;
        anyhow::ensure!(
            warning.contains("--lib"),
            "warning should name --lib, got {warning:?}",
        );
        anyhow::ensure!(
            warning.contains("--bin demo"),
            "warning should name --bin demo, got {warning:?}",
        );
        anyhow::ensure!(
            warning.contains("aggregator")
                || warning.contains("one binary")
                || warning.contains("ignor"),
            "warning should explain why flags were ignored, got {warning:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_absent_returns_empty_and_unchanged_args() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let (forwarded, remaining) = parse_build_forwarder_flags(&input)?;
        anyhow::ensure!(forwarded.is_empty(), "got {forwarded:?}");
        anyhow::ensure!(remaining == input, "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_release_unit_flag() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&["--release", "f"]))?;
        anyhow::ensure!(forwarded == argv(&["--release"]), "got {forwarded:?}");
        anyhow::ensure!(remaining == argv(&["f"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_dash_r_short_for_release() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&["-r", "f"]))?;
        anyhow::ensure!(forwarded == argv(&["-r"]), "got {forwarded:?}");
        anyhow::ensure!(remaining == argv(&["f"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_frozen_locked_offline() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["--frozen", "--locked", "--offline"]))?;
        anyhow::ensure!(
            forwarded == argv(&["--frozen", "--locked", "--offline"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_all_features_and_no_default_features() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["--all-features", "--no-default-features"]))?;
        anyhow::ensure!(
            forwarded == argv(&["--all-features", "--no-default-features"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_keep_going_and_ignore_rust_version() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["--keep-going", "--ignore-rust-version"]))?;
        anyhow::ensure!(
            forwarded == argv(&["--keep-going", "--ignore-rust-version"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_quiet_short_and_long() -> anyhow::Result<()> {
        let (forwarded_short, _) = parse_build_forwarder_flags(&argv(&["-q"]))?;
        let (forwarded_long, _) = parse_build_forwarder_flags(&argv(&["--quiet"]))?;
        anyhow::ensure!(forwarded_short == argv(&["-q"]), "got {forwarded_short:?}");
        anyhow::ensure!(forwarded_long == argv(&["--quiet"]), "got {forwarded_long:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_verbose_short_long_repeatable() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["-v", "--verbose", "-v", "f"]))?;
        anyhow::ensure!(
            forwarded == argv(&["-v", "--verbose", "-v"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining == argv(&["f"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_unit_graph_and_future_incompat() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["--unit-graph", "--future-incompat-report"]))?;
        anyhow::ensure!(
            forwarded == argv(&["--unit-graph", "--future-incompat-report"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_features_space_form() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["--features", "foo,bar", "f"]))?;
        anyhow::ensure!(
            forwarded == argv(&["--features", "foo,bar"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining == argv(&["f"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_features_equals_form_preserves_user_spelling() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&["--features=foo,bar"]))?;
        anyhow::ensure!(forwarded == argv(&["--features=foo,bar"]), "got {forwarded:?}");
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_dash_capital_f_short_for_features_space_form() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["-F", "foo,bar", "f"]))?;
        anyhow::ensure!(forwarded == argv(&["-F", "foo,bar"]), "got {forwarded:?}");
        anyhow::ensure!(remaining == argv(&["f"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_dash_capital_f_short_for_features_equals_form() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&["-F=foo,bar"]))?;
        anyhow::ensure!(forwarded == argv(&["-F=foo,bar"]), "got {forwarded:?}");
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_lockfile_path_space_form() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["--lockfile-path", "alt.lock", "f"]))?;
        anyhow::ensure!(
            forwarded == argv(&["--lockfile-path", "alt.lock"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining == argv(&["f"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_lockfile_path_equals_form_preserves_user_spelling()
    -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["--lockfile-path=alt.lock"]))?;
        anyhow::ensure!(
            forwarded == argv(&["--lockfile-path=alt.lock"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_lockfile_path_missing_value_errors() -> anyhow::Result<()> {
        let Err(err) = parse_build_forwarder_flags(&argv(&["--lockfile-path"])) else {
            anyhow::bail!("expected error for trailing --lockfile-path");
        };
        anyhow::ensure!(
            err.to_string().contains("--lockfile-path"),
            "error should mention the flag, got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_profile_and_target_dir() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&[
            "--profile",
            "ci",
            "--target-dir=/tmp/td",
        ]))?;
        anyhow::ensure!(
            forwarded == argv(&["--profile", "ci", "--target-dir=/tmp/td"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_target_repeatable() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&[
            "--target",
            "x86_64-unknown-linux-gnu",
            "--target=aarch64-apple-darwin",
        ]))?;
        anyhow::ensure!(
            forwarded
                == argv(&[
                    "--target",
                    "x86_64-unknown-linux-gnu",
                    "--target=aarch64-apple-darwin",
                ]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_jobs_short_and_long_both_forms() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&[
            "-j", "8", "--jobs=4", "--jobs", "2", "-j=1",
        ]))?;
        anyhow::ensure!(
            forwarded == argv(&["-j", "8", "--jobs=4", "--jobs", "2", "-j=1"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_message_format_both_forms() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&[
            "--message-format",
            "human",
            "--message-format=json",
        ]))?;
        anyhow::ensure!(
            forwarded
                == argv(&[
                    "--message-format",
                    "human",
                    "--message-format=json",
                ]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_config_repeatable() -> anyhow::Result<()> {
        let (forwarded, remaining) = parse_build_forwarder_flags(&argv(&[
            "--config",
            "build.jobs=8",
            "--config=net.git-fetch-with-cli=true",
        ]))?;
        anyhow::ensure!(
            forwarded
                == argv(&[
                    "--config",
                    "build.jobs=8",
                    "--config=net.git-fetch-with-cli=true",
                ]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_unstable_z_flag() -> anyhow::Result<()> {
        let (forwarded, remaining) =
            parse_build_forwarder_flags(&argv(&["-Z", "unstable-options", "-Z=avoid-dev-deps"]))?;
        anyhow::ensure!(
            forwarded == argv(&["-Z", "unstable-options", "-Z=avoid-dev-deps"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_value_flag_without_value_errors() -> anyhow::Result<()> {
        let Err(err) = parse_build_forwarder_flags(&argv(&["--features"])) else {
            anyhow::bail!("expected error for trailing --features");
        };
        anyhow::ensure!(
            err.to_string().contains("requires a value"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_empty_equals_value_errors() -> anyhow::Result<()> {
        let Err(err) = parse_build_forwarder_flags(&argv(&["--features="])) else {
            anyhow::bail!("expected error for empty --features=");
        };
        anyhow::ensure!(
            err.to_string().contains("non-empty value"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn build_forwarder_mixed_with_runner_args_preserves_order() -> anyhow::Result<()> {
        let input = argv(&[
            "my_filter",
            "--release",
            "--skip",
            "slow_",
            "--features",
            "ci",
            "--output=plain",
        ]);
        let (forwarded, remaining) = parse_build_forwarder_flags(&input)?;
        anyhow::ensure!(
            forwarded == argv(&["--release", "--features", "ci"]),
            "got {forwarded:?}",
        );
        anyhow::ensure!(
            remaining == argv(&["my_filter", "--skip", "slow_", "--output=plain"]),
            "got {remaining:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_records_forwarded_cargo_args() -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["--release", "--features", "ci", "my_filter"]),
            no_dirs,
        )?;
        anyhow::ensure!(
            parsed.forwarded_cargo_args == argv(&["--release", "--features", "ci"]),
            "got {:?}",
            parsed.forwarded_cargo_args,
        );
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_forwarded_default_empty() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&[]), no_dirs)?;
        anyhow::ensure!(
            parsed.forwarded_cargo_args.is_empty(),
            "got {:?}",
            parsed.forwarded_cargo_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_default_inserts_forwarded_before_separator()
    -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["--release", "--features", "ci", "my_filter"]),
            no_dirs,
        )?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        let separator_index = invocation
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| anyhow::anyhow!("expected `--` separator"))?;
        let before: &[String] = invocation.get(..separator_index).unwrap_or(&[]);
        anyhow::ensure!(
            before.contains(&"--release".to_owned()),
            "expected --release before separator, got {before:?}",
        );
        anyhow::ensure!(
            before
                .iter()
                .any(|arg| arg == "--features"),
            "expected --features before separator, got {before:?}",
        );
        let after: &[String] = invocation
            .get(separator_index.saturating_add(1_usize)..)
            .unwrap_or(&[]);
        anyhow::ensure!(
            after == ["my_filter".to_owned()],
            "got {after:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_no_run_inserts_forwarded_with_auto_message_format()
    -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&["--no-run", "--release"]), no_dirs)?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        anyhow::ensure!(
            invocation.contains(&"build".to_owned()),
            "expected build subcommand, got {invocation:?}",
        );
        anyhow::ensure!(
            invocation.contains(&"--release".to_owned()),
            "expected --release forwarded, got {invocation:?}",
        );
        anyhow::ensure!(
            invocation.contains(&"--message-format=json-render-diagnostics".to_owned()),
            "expected auto --message-format injection, got {invocation:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_no_run_skips_auto_message_format_when_user_supplied()
    -> anyhow::Result<()> {
        let parsed =
            parse_test_args(&argv(&["--no-run", "--message-format=human"]), no_dirs)?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        anyhow::ensure!(
            !invocation.contains(&"--message-format=json-render-diagnostics".to_owned()),
            "auto-injection should be skipped when user supplied --message-format, got {invocation:?}",
        );
        anyhow::ensure!(
            invocation.contains(&"--message-format=human".to_owned()),
            "user's --message-format should be present, got {invocation:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_no_run_skips_auto_when_user_supplied_space_form()
    -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["--no-run", "--message-format", "human"]),
            no_dirs,
        )?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        anyhow::ensure!(
            !invocation.contains(&"--message-format=json-render-diagnostics".to_owned()),
            "auto-injection should be skipped when user supplied --message-format, got {invocation:?}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn manifest_path_absent_returns_none_and_unchanged_args() -> anyhow::Result<()> {
        let input = argv(&["my_filter", "--skip", "slow_"]);
        let (manifest, remaining) = parse_manifest_path_flag(&input)?;
        anyhow::ensure!(manifest.is_none(), "got {manifest:?}");
        anyhow::ensure!(remaining == input, "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn manifest_path_space_form_consumed() -> anyhow::Result<()> {
        let input = argv(&["--manifest-path", "/path/to/Cargo.toml", "my_filter"]);
        let (manifest, remaining) = parse_manifest_path_flag(&input)?;
        anyhow::ensure!(
            manifest == Some(Path::new("/path/to/Cargo.toml").to_path_buf()),
            "got {manifest:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn manifest_path_equals_form_consumed() -> anyhow::Result<()> {
        let input = argv(&["--manifest-path=/path/to/Cargo.toml", "my_filter"]);
        let (manifest, remaining) = parse_manifest_path_flag(&input)?;
        anyhow::ensure!(
            manifest == Some(Path::new("/path/to/Cargo.toml").to_path_buf()),
            "got {manifest:?}",
        );
        anyhow::ensure!(remaining == argv(&["my_filter"]), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn manifest_path_last_wins_when_repeated() -> anyhow::Result<()> {
        let input = argv(&[
            "--manifest-path=/first",
            "--manifest-path",
            "/second",
        ]);
        let (manifest, remaining) = parse_manifest_path_flag(&input)?;
        anyhow::ensure!(
            manifest == Some(Path::new("/second").to_path_buf()),
            "expected last-wins, got {manifest:?}",
        );
        anyhow::ensure!(remaining.is_empty(), "got {remaining:?}");
        Ok(())
    }

    #[rudzio::test]
    async fn manifest_path_without_value_errors() -> anyhow::Result<()> {
        let Err(err) = parse_manifest_path_flag(&argv(&["--manifest-path"])) else {
            anyhow::bail!("expected error for trailing --manifest-path");
        };
        anyhow::ensure!(
            err.to_string().contains("requires a path"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn manifest_path_empty_equals_errors() -> anyhow::Result<()> {
        let Err(err) = parse_manifest_path_flag(&argv(&["--manifest-path="])) else {
            anyhow::bail!("expected error for empty --manifest-path=");
        };
        anyhow::ensure!(
            err.to_string().contains("non-empty path"),
            "got {err}",
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_records_manifest_path_in_struct() -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["--manifest-path", "/path/Cargo.toml", "my_filter"]),
            no_dirs,
        )?;
        anyhow::ensure!(
            parsed.manifest_path == Some(Path::new("/path/Cargo.toml").to_path_buf()),
            "got {:?}",
            parsed.manifest_path,
        );
        anyhow::ensure!(
            parsed.runner_args == vec!["my_filter".to_owned()],
            "got {:?}",
            parsed.runner_args,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn parse_test_args_manifest_path_default_none() -> anyhow::Result<()> {
        let parsed = parse_test_args(&argv(&[]), no_dirs)?;
        anyhow::ensure!(
            parsed.manifest_path.is_none(),
            "got {:?}",
            parsed.manifest_path,
        );
        Ok(())
    }

    #[rudzio::test]
    async fn aggregator_cargo_args_default_forwards_runner_args_after_separator()
    -> anyhow::Result<()> {
        let parsed = parse_test_args(
            &argv(&["my_filter", "--skip", "slow_", "--output=plain"]),
            no_dirs,
        )?;
        let invocation = aggregator_cargo_args(&parsed, "/tmp/Cargo.toml");
        let separator_index = invocation
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| anyhow::anyhow!("expected `--` separator in {invocation:?}"))?;
        let after_separator: &[String] = invocation
            .get(separator_index.saturating_add(1_usize)..)
            .unwrap_or(&[]);
        anyhow::ensure!(
            after_separator
                == [
                    "my_filter".to_owned(),
                    "--skip".to_owned(),
                    "slow_".to_owned(),
                    "--output=plain".to_owned(),
                ],
            "got {after_separator:?}",
        );
        Ok(())
    }
}
