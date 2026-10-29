//! Rudzio tests itself: this file runs under `#[rudzio::main]` (via
//! `harness = false`) and every child-process assertion lives inside a
//! `#[rudzio::suite]` module driven by a tokio multi-thread runtime.

use std::fs;
use std::io::Read as _;
use std::io::Result as IoResult;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use cargo_rudzio_test_helpers::{RunSummary, assert_exact_test_names, assert_summary};
use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::futures::ThreadPool;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
#[cfg(unix)]
use rustix::process::{Pid, Signal, kill_process};

/// RAII cleanup of the temp log file the Bug #13 fixture writes to.
/// Kept at module scope (not nested inside the test fn) per clippy's
/// `items_after_statements`.
#[cfg(unix)]
struct LogPathGuard {
    /// Path to the temp log file the fixture wrote.
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for LogPathGuard {
    fn drop(&mut self) {
        let _unused = fs::remove_file(&self.path);
    }
}

#[cfg(not(unix))]
#[expect(
    dead_code,
    reason = "non-Unix builds skip the Bug #13 capture-pipe regression test; the guard is Unix-only"
)]
struct LogPathGuard;

/// Find `needle` in `hay` and return its byte index, or an error
/// naming the missing needle. Used by `ordered` for stdio-attribution
/// stream-order checks.
fn index_of(hay: &str, needle: &str) -> anyhow::Result<usize> {
    hay.find(needle)
        .ok_or_else(|| anyhow::anyhow!("`{needle}` missing"))
}

/// Combine the child's stdout and stderr into one buffer (framework
/// diagnostics show up on both streams in plain mode).
fn log_of(out: &Output) -> String {
    let mut buf = String::from_utf8_lossy(&out.stdout).into_owned();
    buf.push_str(&String::from_utf8_lossy(&out.stderr));
    buf
}

/// Assert that each entry of `needles` appears in `hay` in the given
/// order (the byte index of every needle is monotonically increasing).
fn ordered(hay: &str, needles: &[&str]) -> anyhow::Result<()> {
    let mut last: usize = 0_usize;
    for needle in needles {
        let pos =
            index_of(hay, needle).map_err(|err| anyhow::anyhow!("{err} in stream:\n{hay}"))?;
        anyhow::ensure!(
            pos >= last,
            "`{needle}` found at {pos} but expected at >= {last} (previous neighbour). Stream:\n{hay}",
        );
        last = pos;
    }
    Ok(())
}

/// Run the fixture binary at `exe` with `NO_COLOR=1`, returning its
/// captured stdout+stderr `Output` or an error explaining the spawn
/// failure.
fn run(exe: &str) -> anyhow::Result<Output> {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .output()
        .with_context(|| format!("failed to spawn {exe}"))
}

/// Run the fixture binary `exe` and once it prints `ready_marker` on
/// stdout, deliver `signal` to it. Combined stdout+stderr is returned
/// in `Output`. Unix-only; the fixture must print a deterministic
/// readiness marker so the integration test can race-free signal it.
#[cfg(unix)]
fn run_and_signal(exe: &str, ready_marker: &str, signal: Signal) -> anyhow::Result<Output> {
    use std::os::unix::process::ExitStatusExt as _;

    let mut child = Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {exe}"))?;

    // Read stdout incrementally until we see the readiness marker.
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout missing"))?;
    let mut stdout_buf: Vec<u8> = Vec::new();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(10_u64))
        .ok_or_else(|| anyhow::anyhow!("deadline overflow"))?;
    let mut tmp = [0_u8; 256_usize];
    while !String::from_utf8_lossy(&stdout_buf).contains(ready_marker) {
        if Instant::now() >= deadline {
            let _killed: IoResult<()> = child.kill();
            let _waited: IoResult<ExitStatus> = child.wait();
            anyhow::bail!("readiness marker {ready_marker:?} never appeared");
        }
        match stdout.read(&mut tmp) {
            Ok(0) => break,
            Ok(read) => {
                let slice = tmp
                    .get(..read)
                    .ok_or_else(|| anyhow::anyhow!("child read returned {read} > buf len"))?;
                stdout_buf.extend_from_slice(slice);
            }
            Err(err) => anyhow::bail!("read child stdout: {err}"),
        }
    }

    // Small grace for signal_hook's `Signals::new` to finish its
    // internal setup after returning. The `ready_marker` itself
    // proves the runner is past `install_signal_handler`, so the
    // OS handler is registered; this 100ms is a safety margin for
    // any post-return setup signal_hook does internally.
    thread::sleep(Duration::from_millis(100_u64));

    let pid_raw = i32::try_from(child.id())
        .with_context(|| format!("child pid {} does not fit in pid_t", child.id()))?;
    let pid =
        Pid::from_raw(pid_raw).ok_or_else(|| anyhow::anyhow!("child pid 0 is not a valid pid"))?;
    kill_process(pid, signal).with_context(|| format!("kill({signal:?})"))?;

    // Drain the rest of stdout concurrently with stderr.
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stderr missing"))?;
    let stderr_handle = thread::spawn(move || -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let _read: IoResult<usize> = stderr.read_to_end(&mut buf);
        buf
    });
    let _read: IoResult<usize> = stdout.read_to_end(&mut stdout_buf);
    let stderr_buf = stderr_handle
        .join()
        .map_err(|_join_err| anyhow::anyhow!("stderr thread panicked"))?;

    // Poll for child exit with a generous deadline. If the runner's
    // signal handler is starved (e.g. under saturated CPU contention
    // from the full-suite aggregator), the subprocess may hang
    // indefinitely on its `pending()` body waiting for cancellation
    // that never arrives. The deadline ensures the parent test
    // doesn't hang too — kill the child and surface a clear failure
    // instead of timing out at the test-runner level.
    let exit_deadline = Instant::now()
        .checked_add(Duration::from_mins(2))
        .ok_or_else(|| anyhow::anyhow!("exit deadline overflow"))?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= exit_deadline {
            let _killed: IoResult<()> = child.kill();
            let waited = child.wait().context("wait killed child")?;
            anyhow::bail!(
                "child did not exit within deadline after signal {signal:?}; \
                 killed via SIGKILL. Final status: {:?}",
                waited.code()
            );
        }
        thread::sleep(Duration::from_millis(50_u64));
    };
    Ok(Output {
        status: ExitStatus::from_raw(status.into_raw()),
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Run the fixture binary at `exe` with `RUST_TEST_THREADS=1` (forces
/// strict serial dispatch) and return its captured `Output`.
fn run_serial(exe: &str) -> anyhow::Result<Output> {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", "1")
        .output()
        .with_context(|| format!("failed to spawn {exe}"))
}

/// Run the fixture binary at `exe` with `RUST_TEST_THREADS=1` and the
/// given extra `args`, returning its captured `Output`.
fn run_serial_with_args(exe: &str, args: &[&str]) -> anyhow::Result<Output> {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", "1")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn {exe}"))
}

/// Run the fixture binary at `exe` with `RUST_TEST_THREADS=threads` so
/// the runner dispatches up to `threads` tests concurrently.
fn run_parallel(exe: &str, threads: u32) -> anyhow::Result<Output> {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", threads.to_string())
        .output()
        .with_context(|| format!("failed to spawn {exe}"))
}

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod tests {
    use super::{
        Command, Duration, ExitStatus, Instant, IoResult, RunSummary, Test,
        assert_exact_test_names, assert_summary, log_of, run, run_parallel, run_serial,
        run_serial_with_args, thread,
    };

    #[rudzio::test]
    fn mut_test_context_is_borrowable(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_mutable_test_context"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 3,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "mutates_via_mut_borrow",
                "sync_mutates_via_mut_borrow",
                "fresh_ctx_per_test",
            ],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn passing_tokio_mt_succeeds(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["first_passes", "second_passes"])?;
        Ok(())
    }

    /// Bug #5: `#[rudzio::suite]` must register tests generated by
    /// `macro_rules!`. The macro-generated tests (`macro_generated_a`,
    /// `macro_generated_b`) run and pass alongside a direct `direct_fn` in
    /// the same module; `macro_generated_ignored` is reported ignored.
    #[rudzio::test]
    fn macro_rules_generated_tests_run(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_macro_rules_generated_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 3,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 1,
                teardown_failed: 0,
                measured: 0,
                total: 4,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "macro_generated_a",
                "macro_generated_b",
                "direct_fn",
                "macro_generated_ignored",
            ],
        )?;
        Ok(())
    }

    /// Bug #5 edge cases: every `macro_rules!`-generated shape — same body
    /// under different names, two fns per invocation, nested macros, a
    /// crate-level macro, `&mut Test` and no-context parameters, and a
    /// non-test helper emitted alongside the test — must register and run.
    #[rudzio::test]
    fn macro_rules_edge_cases_all_register(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_macro_rules_edge_cases_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 9,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 9,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "same_body_a",
                "same_body_b",
                "same_body_c",
                "pair_first",
                "pair_second",
                "nested_inner",
                "mut_ctx_test",
                "no_ctx_test",
                "calls_helper",
            ],
        )?;
        Ok(())
    }

    /// Bug #5 edge case: `#[rudzio::test(...)]` attribute arguments emitted
    /// through `macro_rules!` — `#[ignore]` reports ignored, `timeout = 1`
    /// fires the per-test body timeout.
    #[rudzio::test]
    fn macro_rules_attr_args_round_trip(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_macro_rules_attr_args_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 (one timed-out test), output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 0,
                panicked: 0,
                timed_out: 1,
                cancelled: 0,
                ignored: 1,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["attr_ignored", "attr_timeout"])?;
        Ok(())
    }

    /// Bug #5 edge case: the same macro invoked in two suites with different
    /// runtimes must register the identically-named test once per suite
    /// config (two total runs), never cross-registering into the sibling
    /// suite's config (which would yield four).
    #[rudzio::test]
    fn macro_rules_two_suites_not_cross_registered(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_macro_rules_two_suites"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("tokio::Multithread"), "output:\n{log}");
        anyhow::ensure!(log.contains("tokio::CurrentThread"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        // `shared_test` runs once per suite config (two total), never
        // cross-registered into the sibling suite (which would yield four).
        assert_exact_test_names(&log, &["shared_test", "shared_test"])?;
        Ok(())
    }

    /// Bug #5 edge case: a `#[cfg(feature = "...")]`-gated macro invocation.
    /// cfg-stripping runs before the suite attribute macro, so the gated
    /// invocation vanishes when the feature is off (its test must not
    /// register) and survives when on (its test registers). The unconditional
    /// `always_on` invocation keeps the module valid in both states. The
    /// assertion branches on `cfg!(feature = ...)` — a runtime check — so a
    /// single (non-cfg-gated) test fn covers both feature states without the
    /// suite macro's cfg-gated-direct-fn limitation.
    #[rudzio::test]
    fn macro_rules_cfg_gated_test_matches_feature_state(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_macro_rules_cfg_gated_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("always_on"), "output:\n{log}");
        if cfg!(feature = "edge-cfg-macro") {
            anyhow::ensure!(
                log.contains("cfg_gated_on"),
                "feature on: gated test must register, output:\n{log}"
            );
            assert_summary(
                &log,
                RunSummary {
                    passed: 2,
                    failed: 0,
                    panicked: 0,
                    timed_out: 0,
                    cancelled: 0,
                    ignored: 0,
                    teardown_failed: 0,
                    measured: 0,
                    total: 2,
                    filtered_out: 0,
                },
            )?;
            assert_exact_test_names(&log, &["always_on", "cfg_gated_on"])?;
        } else {
            anyhow::ensure!(
                !log.contains("cfg_gated_on"),
                "feature off: gated test must be absent, output:\n{log}"
            );
            assert_summary(
                &log,
                RunSummary {
                    passed: 1,
                    failed: 0,
                    panicked: 0,
                    timed_out: 0,
                    cancelled: 0,
                    ignored: 0,
                    teardown_failed: 0,
                    measured: 0,
                    total: 1,
                    filtered_out: 0,
                },
            )?;
            assert_exact_test_names(&log, &["always_on"])?;
        }
        Ok(())
    }

    /// A direct `#[rudzio::test]` fn carrying its own `#[cfg(feature =
    /// "never-enabled")]` (off) is stripped; only the cfg-true and ungated
    /// fns remain registered. `never-enabled` is never enabled, so the
    /// assertion is deterministic.
    #[rudzio::test]
    fn direct_cfg_gated_tests_run_only_cfg_true_and_ungated(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_direct_cfg_gated_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("cfg_true_test"), "output:\n{log}");
        anyhow::ensure!(log.contains("ungated_test"), "output:\n{log}");
        anyhow::ensure!(
            !log.contains("cfg_false_test"),
            "cfg-false test must be absent, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["cfg_true_test", "ungated_test"])?;
        Ok(())
    }

    /// A `macro_rules!`-generated test fn carrying `#[cfg(feature =
    /// "never-enabled")]` (off) directly on the fn is stripped; its
    /// `__register_test!` registration items must carry the cfg too.
    #[rudzio::test]
    fn macro_rules_fn_cfg_gated_test_is_stripped(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_macro_rules_fn_cfg_gated_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("always_on"), "output:\n{log}");
        anyhow::ensure!(
            !log.contains("cfg_gated_off"),
            "cfg-gated fn must be absent, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["always_on"])?;
        Ok(())
    }

    /// `#[cfg_attr(cond, ignore)]` must be respected: `all()` (always true)
    /// ignores the test; `any()` (always false) leaves it running.
    #[rudzio::test]
    fn cfg_attr_ignore_is_respected(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_cfg_attr_ignore_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("cfg_attr_ignored"), "output:\n{log}");
        anyhow::ensure!(log.contains("cfg_attr_runs"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 1,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["cfg_attr_ignored", "cfg_attr_runs"])?;
        Ok(())
    }

    /// Raw-identifier test names (`fn r#type`, `fn r#match`) must register
    /// and run. The suite codegen derives the hidden token-static and
    /// run-test-helper idents from the test ident's string form; a raw ident
    /// carries a `r#` prefix that must be stripped or `format_ident!` panics.
    #[rudzio::test]
    fn raw_ident_test_names_register(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_raw_ident_test_names_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 3,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["r#type", "r#match", "ordinary_name"])?;
        Ok(())
    }

    /// A `#[rudzio::suite]` mod nested inside another `#[rudzio::suite]` mod.
    /// The outer suite must pass the still-attributed inner mod through its
    /// body and register only its own direct test; the inner suite registers
    /// its own. Both run under a single `#[rudzio::main]`.
    #[rudzio::test]
    fn nested_suite_registers_both_levels(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_nested_suite_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("outer_direct"), "output:\n{log}");
        anyhow::ensure!(log.contains("inner_direct"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["outer_direct", "outer::inner_direct"])?;
        Ok(())
    }

    /// A `#[cfg(feature = "...")]`-gated suite *mod*. cfg-stripping runs
    /// before attribute-macro expansion, so the gated suite vanishes when the
    /// feature is off (its test must not register) and survives when on.
    /// Mirrors `macro_rules_cfg_gated_tokio_mt`'s runtime-branch assertion.
    #[rudzio::test]
    fn cfg_gated_suite_mod_matches_feature_state(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_cfg_gated_suite_mod_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("always_on"), "output:\n{log}");
        if cfg!(feature = "edge-cfg-macro") {
            anyhow::ensure!(
                log.contains("cfg_gated_suite_test"),
                "feature on: gated suite must register, output:\n{log}"
            );
            assert_summary(
                &log,
                RunSummary {
                    passed: 2,
                    failed: 0,
                    panicked: 0,
                    timed_out: 0,
                    cancelled: 0,
                    ignored: 0,
                    teardown_failed: 0,
                    measured: 0,
                    total: 2,
                    filtered_out: 0,
                },
            )?;
            assert_exact_test_names(&log, &["always_on", "cfg_gated_suite_test"])?;
        } else {
            anyhow::ensure!(
                !log.contains("cfg_gated_suite_test"),
                "feature off: gated suite must be absent, output:\n{log}"
            );
            assert_summary(
                &log,
                RunSummary {
                    passed: 1,
                    failed: 0,
                    panicked: 0,
                    timed_out: 0,
                    cancelled: 0,
                    ignored: 0,
                    teardown_failed: 0,
                    measured: 0,
                    total: 1,
                    filtered_out: 0,
                },
            )?;
            assert_exact_test_names(&log, &["always_on"])?;
        }
        Ok(())
    }

    /// `#[rudzio::main]` on a fn with an explicit
    /// `-> ::std::process::ExitCode` return type must be tolerated: the macro
    /// discards the user's signature and always emits its own
    /// `fn main() -> ExitCode` wrapper.
    #[rudzio::test]
    fn main_explicit_exitcode_return_type_works(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_main_explicit_exitcode_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("passes_under_explicit_exitcode"),
            "output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["passes_under_explicit_exitcode"])?;
        Ok(())
    }

    /// `#[cfg_attr(cond, ignore = "...")]` emitted through `macro_rules!` must
    /// round-trip through the forwarding macro and be respected: the generated
    /// test is reported ignored with its reason.
    #[rudzio::test]
    fn cfg_attr_ignore_via_macro_rules_is_respected(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_cfg_attr_ignore_macro_rules_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("cfg_attr_ignored_via_macro"), "output:\n{log}");
        anyhow::ensure!(
            log.contains("macro-generated cfg_attr ignore"),
            "ignore reason must be reported, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 1,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["cfg_attr_ignored_via_macro"])?;
        Ok(())
    }

    #[rudzio::test]
    fn failing_tokio_mt_exits_one(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_failing_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 1,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["passes", "fails"])?;
        Ok(())
    }

    #[rudzio::test]
    fn ignored_tests_are_skipped(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_ignored_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[IGNORE]"), "output:\n{log}");
        anyhow::ensure!(log.contains("takes too long"), "output:\n{log}");
        anyhow::ensure!(!log.contains("must not run"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 2,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["runs", "ignored_bare", "ignored_with_reason"])?;
        Ok(())
    }

    #[rudzio::test]
    fn tokio_current_thread_runtime_works(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_tokio_ct"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("yields_then_passes"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["yields_then_passes"])?;
        Ok(())
    }

    #[rudzio::test]
    fn compio_runtime_works(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_compio"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("passes_under_compio"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["passes_under_compio"])?;
        Ok(())
    }

    #[rudzio::test]
    fn futures_runtime_works(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_futures"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("futures::ThreadPool"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["passes_under_futures", "spawn_works_under_futures"])?;
        Ok(())
    }

    #[rudzio::test]
    fn multi_runtime_runs_every_config(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_multi_runtime"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("Multithread"), "output:\n{log}");
        anyhow::ensure!(log.contains("compio::Runtime"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["runs_on_every_runtime", "runs_on_every_runtime"])?;
        Ok(())
    }

    #[rudzio::test]
    fn panicking_test_does_not_halt_the_suite(_ctx: &Test) -> anyhow::Result<()> {
        // Expected behavior: a panic in one test is isolated — subsequent tests
        // in the same runtime group still execute and the summary reports
        // 2 passed, 1 panicked, 3 total.
        let out = run(env!("CARGO_BIN_EXE_panics_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(log.contains("before_panic"), "output:\n{log}");
        anyhow::ensure!(
            log.contains("after_panic"),
            "after_panic never ran — panic killed the whole thread. output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 1,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["before_panic", "panics", "after_panic"])?;
        Ok(())
    }

    #[rudzio::test]
    fn sync_tests_mix_pass_fail_panic(_ctx: &Test) -> anyhow::Result<()> {
        // All three sync tests run; isolation keeps panics from killing the thread.
        let out = run(env!("CARGO_BIN_EXE_sync_mixed_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("sync_passes"), "output:\n{log}");
        anyhow::ensure!(log.contains("sync_returns_err"), "output:\n{log}");
        anyhow::ensure!(log.contains("sync_panics"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 1,
                panicked: 1,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["sync_passes", "sync_returns_err", "sync_panics"])?;
        Ok(())
    }

    #[rudzio::test]
    fn custom_suite_and_test_impls_work(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_custom_context_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("runs_on_custom_context"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["runs_on_custom_context"])?;
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_failure_aborts_the_runtime_group(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_setup_failure_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        // The new lifecycle line + the error's Display must both appear.
        anyhow::ensure!(log.contains("[FAIL]"), "output:\n{log}");
        anyhow::ensure!(log.contains("setup "), "output:\n{log}");
        anyhow::ensure!(log.contains("setup_failed_by_design"), "output:\n{log}");
        // Tests that never ran are reported as Cancelled, not Panicked.
        // No test emits an [OK] tag; setup failed before teardown ran, so
        // the [OK] teardown line never gets emitted either.
        anyhow::ensure!(!log.contains("[OK]"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 1,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["never_runs"])?;
        Ok(())
    }

    #[rudzio::test]
    fn context_creation_failure_counts_as_failed(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_context_creation_failure_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("first"), "output:\n{log}");
        anyhow::ensure!(log.contains("second"), "output:\n{log}");
        // Per-test context failures get the distinct [SETUP] status tag
        // so they're visually different from a regular [FAIL].
        anyhow::ensure!(log.contains("[SETUP]"), "output:\n{log}");
        // The error's Display must be propagated through the failure output.
        anyhow::ensure!(
            log.contains("context_creation_failed_by_design"),
            "output:\n{log}"
        );
        // SetupFailed counts toward the `failed` bucket (it's a kind of
        // failure), preserving the summary-stat semantics.
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 2,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["first", "second"])?;
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_and_teardown_lines_appear_in_passing_run(_ctx: &Test) -> anyhow::Result<()> {
        // A normal passing run must emit visible setup/teardown
        // lifecycle lines so the user knows the suite phases happened
        // (the whole point of the new output: see *that* it's
        // happening, not just *whether* it failed).
        let out = run(env!("CARGO_BIN_EXE_passing_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        // "started" lines for setup and teardown both fire in plain
        // mode regardless of outcome.
        anyhow::ensure!(
            log.contains("setup ") && log.contains("started"),
            "missing setup started line:\n{log}"
        );
        anyhow::ensure!(
            log.contains("teardown ") && log.contains("started"),
            "missing teardown started line:\n{log}"
        );
        // And on success both phases close with an [OK] line.
        anyhow::ensure!(
            log.matches("[OK]").count() >= 2,
            "expected at least 2 [OK] occurrences (setup + teardown), got:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["first_passes", "second_passes"])?;
        Ok(())
    }

    #[rudzio::test]
    fn suite_teardown_failure_is_reported(_ctx: &Test) -> anyhow::Result<()> {
        // Teardown errors used to print as a one-line `warning:` and
        // were easy to miss. Now they emit a `[FAIL] teardown` line
        // carrying the error message and contribute to the
        // teardown_failures count, which drives the run's exit code.
        let out = run(env!("CARGO_BIN_EXE_teardown_failure_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 (teardown failure fails the run), output:\n{log}"
        );
        // Setup succeeded → an [OK] line for setup must be present.
        anyhow::ensure!(
            log.contains("[OK]") && log.contains("setup "),
            "missing setup OK line:\n{log}"
        );
        // The teardown failure line + error message must both appear.
        anyhow::ensure!(log.contains("[FAIL]"), "output:\n{log}");
        anyhow::ensure!(log.contains("teardown "), "output:\n{log}");
        anyhow::ensure!(log.contains("teardown_failed_by_design"), "output:\n{log}");
        // The test body itself ran successfully.
        anyhow::ensure!(
            log.contains("body_runs_then_teardown_fails"),
            "output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 1,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_runs_then_teardown_fails"])?;
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        // catch_unwind wrapper around Suite::setup turns the panic
        // into a structured `[FAIL] setup` line carrying the panic
        // message instead of unwinding through the runtime thread
        // (which would print the generic "runtime thread panicked").
        let out = run(env!("CARGO_BIN_EXE_panic_in_suite_setup_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[FAIL]"), "missing FAIL tag:\n{log}");
        anyhow::ensure!(log.contains("setup "), "missing setup line:\n{log}");
        anyhow::ensure!(
            log.contains("suite_setup_panicked_by_design"),
            "panic message must propagate, output:\n{log}"
        );
        // Should NOT see the generic thread-panic diagnostic from the
        // runner's catch-all — that would mean catch_unwind didn't fire.
        anyhow::ensure!(
            !log.contains("runtime thread panicked"),
            "panic escaped catch_unwind, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 1,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["never_runs"])?;
        Ok(())
    }

    #[rudzio::test]
    fn suite_teardown_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_panic_in_suite_teardown_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        // Setup ran ok; body ran ok; teardown panicked.
        anyhow::ensure!(
            log.contains("[OK]") && log.contains("setup "),
            "missing setup OK line:\n{log}"
        );
        anyhow::ensure!(log.contains("[PANIC]"), "missing PANIC tag:\n{log}");
        anyhow::ensure!(log.contains("teardown "), "missing teardown line:\n{log}");
        anyhow::ensure!(
            log.contains("suite_teardown_panicked_by_design"),
            "panic message must propagate, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("runtime thread panicked"),
            "panic escaped catch_unwind, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 1,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_runs_then_teardown_panics"])?;
        Ok(())
    }

    #[rudzio::test]
    fn test_setup_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        // catch_unwind around `Suite::context` turns a per-test setup
        // panic into a `TestOutcome::SetupFailed` carrying the panic
        // message — rendered with the `[SETUP]` status tag.
        let out = run(env!("CARGO_BIN_EXE_panic_in_test_setup_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[SETUP]"), "missing SETUP tag:\n{log}");
        anyhow::ensure!(log.contains("body_never_runs"), "output:\n{log}");
        anyhow::ensure!(
            log.contains("test_setup_panicked_by_design"),
            "panic message must propagate, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("runtime thread panicked"),
            "panic escaped catch_unwind, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 1,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_never_runs"])?;
        Ok(())
    }

    #[rudzio::test]
    fn test_teardown_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        // catch_unwind around `Test::teardown` routes the panic
        // through the structured `report_test_teardown_failure`
        // method (no `report_warning` escape hatch), bumps the
        // per-test teardown counter, and the run exits non-zero.
        let out = run(env!("CARGO_BIN_EXE_panic_in_test_teardown_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[PANIC]"), "missing PANIC tag:\n{log}");
        anyhow::ensure!(
            log.contains("body_runs_then_teardown_panics"),
            "output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("test_teardown_panicked_by_design"),
            "panic message must propagate, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("runtime thread panicked"),
            "panic escaped catch_unwind, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 1,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_runs_then_teardown_panics"])?;
        Ok(())
    }

    #[rudzio::test]
    fn plain_test_attribute_is_recognized(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_plain_test_attr_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("runs_via_plain_test_attribute"),
            "output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["runs_via_plain_test_attribute"])?;
        Ok(())
    }

    #[rudzio::test]
    fn bare_return_types_compile_and_pass(_ctx: &Test) -> anyhow::Result<()> {
        // Captures the promise that `#[rudzio::test]` handles every
        // return shape `#[test]` / `#[tokio::test]` accept — users
        // don't have to rewrite void bodies into `-> anyhow::Result`.
        // If `rudzio::IntoRudzioResult` loses an impl, this fixture
        // stops compiling; regressions surface at the PR level.
        let out = run(env!("CARGO_BIN_EXE_bare_return_types_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 5,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 5,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "bare_sync_void",
                "explicit_unit_return",
                "bare_async_void",
                "result_returning",
                "result_display_only",
            ],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn multiple_panics_are_isolated_and_ordered(_ctx: &Test) -> anyhow::Result<()> {
        // RUST_TEST_THREADS=1 forces strict serial execution so the
        // source-order assertion below is meaningful.
        let out = run_serial(env!("CARGO_BIN_EXE_multi_panic_ordering_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 3,
                failed: 0,
                panicked: 2,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 5,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "step_1_pass",
                "step_2_panic",
                "step_3_pass",
                "step_4_panic",
                "step_5_pass",
            ],
        )?;

        // Sequential source-order execution: each test name must come before the next.
        let mut positions: Vec<usize> = Vec::new();
        for name in [
            "step_1_pass",
            "step_2_panic",
            "step_3_pass",
            "step_4_panic",
            "step_5_pass",
        ] {
            let pos = log
                .find(name)
                .ok_or_else(|| anyhow::anyhow!("missing {name} in output:\n{log}"))?;
            positions.push(pos);
        }
        anyhow::ensure!(
            positions
                .windows(2_usize)
                .all(|win| win.first() < win.get(1_usize)),
            "tests ran out of order; positions {positions:?}, output:\n{log}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn tracker_drains_on_suite_teardown(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_tracker_drain_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        // The marker only appears if suite teardown waited for the tracked task.
        anyhow::ensure!(
            log.contains("tracker_drain_marker"),
            "tracker did not drain the spawned task; output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["returns_before_tracked_task_completes"])?;
        Ok(())
    }

    #[rudzio::test]
    fn per_test_teardown_cancels_the_cancel_token(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_cancel_token_propagation_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("cancel_propagation_marker"),
            "teardown did not cancel the token; output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["cancel_token_wakes_on_teardown"])?;
        Ok(())
    }

    #[rudzio::test]
    fn spawn_tracked_test_passes(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_spawn_tracked"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("spawn_awaits_result"), "output:\n{log}");
        anyhow::ensure!(log.contains("cancel_token_is_child"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["spawn_awaits_result", "cancel_token_is_child"])?;
        Ok(())
    }

    #[rudzio::test]
    fn sync_work_runs_on_the_runtimes_blocking_pool(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_spawn_blocking_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("runs_sync_fn_via_spawn_blocking"),
            "output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("spawn_blocking_uses_a_different_thread"),
            "output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "runs_sync_fn_via_spawn_blocking",
                "spawn_blocking_uses_a_different_thread",
            ],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn parallel_runner_executes_tests_concurrently(_ctx: &Test) -> anyhow::Result<()> {
        // The fixture has three tests that synchronise on a Barrier::new(3) with a 2s timeout.
        // If the runner dispatches all three concurrently, the barrier releases and every test passes.
        let out = run_parallel(env!("CARGO_BIN_EXE_parallel_tokio_mt"), 3)?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0 under parallel dispatch, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 3,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "first_hits_barrier",
                "second_hits_barrier",
                "third_hits_barrier",
            ],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn multiple_runtime_groups_run_in_separate_threads_concurrently(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        // Fixture has two runtime groups (tokio multi-thread + compio), each with one test.
        // Both tests block on a shared Barrier(2) via spawn_blocking. If the groups' threads
        // run in parallel, both arrive at the barrier and it releases.
        let out = run(env!("CARGO_BIN_EXE_cross_runtime_parallel"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0 (watchdog exit 2 means groups serialised), output:\n{log}"
        );
        anyhow::ensure!(log.contains("Multithread"), "output:\n{log}");
        anyhow::ensure!(log.contains("compio::Runtime"), "output:\n{log}");
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "waits_on_cross_runtime_barrier",
                "waits_on_cross_runtime_barrier",
            ],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn teardown_runs_even_on_per_test_timeout(_ctx: &Test) -> anyhow::Result<()> {
        // `--test-timeout=1` forces the test body to be cancelled by the
        // runner's watchdog. Both the per-test teardown and the suite teardown
        // must still run — the integration asserts on their stdout markers.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_teardown_always_runs_tokio_mt"),
            &["--test-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[TIMEOUT]"), "output:\n{log}");
        anyhow::ensure!(
            !log.contains("body_times_out_unreached_marker"),
            "test body must not complete, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("teardown_test_marker"),
            "per-test teardown must run after timeout, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("teardown_suite_marker"),
            "suite teardown must run after timeout, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 0,
                panicked: 0,
                timed_out: 1,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_times_out"])?;
        Ok(())
    }

    #[rudzio::test]
    fn per_test_timeout_cancels_only_the_offending_test(_ctx: &Test) -> anyhow::Result<()> {
        // With `--test-timeout=1`, the per-test watchdog must fire only on the
        // hanging test; the other test in the same suite must still run.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_timeout_tokio_mt"),
            &["--test-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 on per-test timeout, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[TIMEOUT]"), "output:\n{log}");
        anyhow::ensure!(
            !log.contains("hangs_until_timeout_unreached_marker"),
            "test body should not have completed, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("still_runs_after_previous_timeout_marker"),
            "subsequent test must still run, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 1,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &["hangs_until_timeout", "still_runs_after_previous_timeout"],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn run_timeout_cancels_the_whole_run(_ctx: &Test) -> anyhow::Result<()> {
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_run_timeout_tokio_mt"),
            &["--run-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 after run-timeout cancel, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("run timeout"),
            "expected a run-timeout diagnostic, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("waits_for_run_cancel_acknowledged_marker"),
            "first test should observe cancellation, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[CANCEL]") && log.contains("never_starts_first"),
            "output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[CANCEL]") && log.contains("never_starts_second"),
            "output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("never_starts_first_unreached_marker"),
            "cancelled test body must not run, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 2,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "waits_for_run_cancel",
                "never_starts_first",
                "never_starts_second",
            ],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_timeout_cancels_every_test(_ctx: &Test) -> anyhow::Result<()> {
        // `--suite-setup-timeout=1` fires while suite setup hangs; every
        // queued test reports as cancelled and the binary exits non-zero.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_suite_setup_timeout_tokio_mt"),
            &["--suite-setup-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("setup timed out"),
            "expected setup-timeout diagnostic, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[CANCEL]") && log.contains("never_runs"),
            "queued test must report cancelled, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("hanging_suite_setup_unreached_marker"),
            "setup body must not run to completion, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("teardown_must_not_run_marker"),
            "suite teardown must not run when setup never finished, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 1,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["never_runs"])?;
        Ok(())
    }

    #[rudzio::test]
    fn suite_teardown_timeout_is_reported(_ctx: &Test) -> anyhow::Result<()> {
        // Body passes; `--suite-teardown-timeout=1` fires; the lifecycle
        // line for teardown carries the `[TIMEOUT]` tag and the run is
        // failed via `teardown_failures`.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_suite_teardown_timeout_tokio_mt"),
            &["--suite-teardown-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[TIMEOUT]") && log.contains("teardown"),
            "expected [TIMEOUT] teardown line, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("hanging_suite_teardown_unreached_marker"),
            "teardown future must be dropped, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 1,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_passes_then_suite_teardown_times_out"])?;
        Ok(())
    }

    #[rudzio::test]
    fn test_setup_timeout_marks_test_as_setup_failed(_ctx: &Test) -> anyhow::Result<()> {
        // `--test-setup-timeout=1` fires while `Suite::context` hangs.
        // Test outcome is `[SETUP]` (per-test setup failed), per-test
        // teardown does NOT run because no context was constructed.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_test_setup_timeout_tokio_mt"),
            &["--test-setup-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[SETUP]") && log.contains("body_never_runs"),
            "expected [SETUP] outcome, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("setup timed out"),
            "expected setup-timeout diagnostic, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("test_teardown_must_not_run_marker"),
            "per-test teardown must skip when context never built, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 1,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_never_runs"])?;
        Ok(())
    }

    #[rudzio::test]
    fn test_teardown_timeout_is_reported_per_test(_ctx: &Test) -> anyhow::Result<()> {
        // `--test-teardown-timeout=1` fires while `Test::teardown` hangs.
        // Body passes; teardown line shows `[TIMEOUT]` and contributes
        // to teardown_failures.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_test_teardown_timeout_tokio_mt"),
            &["--test-teardown-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[TIMEOUT]") && log.contains("teardown"),
            "expected [TIMEOUT] teardown line, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("hanging_test_teardown_unreached_marker"),
            "teardown future must be dropped, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 1,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["body_passes_then_teardown_times_out"])?;
        Ok(())
    }

    #[rudzio::test]
    fn per_test_attr_body_timeout_overrides_default(_ctx: &Test) -> anyhow::Result<()> {
        // No CLI timeouts. The annotated test (`#[rudzio::test(timeout = 1)]`)
        // times out via attribute alone; sibling test stays unbounded.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_attr_body_timeout_tokio_mt"),
            &[],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 from attribute-driven body timeout, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[TIMEOUT]"), "output:\n{log}");
        anyhow::ensure!(
            log.contains("attr_body_times_out"),
            "expected attr-targeted test name, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("unbounded_sibling_marker"),
            "sibling test must run unbounded, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 1,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &["attr_body_times_out", "unbounded_sibling_still_runs"],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn per_test_attr_setup_timeout_overrides_default(_ctx: &Test) -> anyhow::Result<()> {
        // No CLI timeouts. Setup phase times out only because of the
        // attribute override on this test.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_attr_setup_timeout_tokio_mt"),
            &[],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[SETUP]") && log.contains("attr_setup_times_out"),
            "expected [SETUP] outcome on the targeted test, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("setup timed out"),
            "expected setup-timeout diagnostic, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 0,
                failed: 1,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["attr_setup_times_out"])?;
        Ok(())
    }

    #[rudzio::test]
    fn per_test_attr_teardown_timeout_overrides_default(_ctx: &Test) -> anyhow::Result<()> {
        // No CLI timeouts. Teardown phase times out only because of the
        // attribute override on this test. Body still passes.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_attr_teardown_timeout_tokio_mt"),
            &[],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[TIMEOUT]") && log.contains("teardown"),
            "expected [TIMEOUT] teardown line, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 1,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["attr_teardown_times_out"])?;
        Ok(())
    }

    #[rudzio::test]
    fn bg_thread_panic_during_setup_flips_exit_code(_ctx: &Test) -> anyhow::Result<()> {
        // Real-world repro: rustls / aws-smithy spawn a thread that
        // panics during crypto-provider init while the main setup
        // future returns Ok. Without the panic-hook safety net the
        // binary exits 0 and the summary lies. With it, the runner
        // detects the unattributed panic, prints a warning, and exits
        // 1.
        let out = run_serial(env!("CARGO_BIN_EXE_bg_thread_panic_in_setup_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 from bg-panic safety net, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("simulated_bg_thread_panic_during_setup"),
            "user's panic message must appear, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("background-thread panic"),
            "expected rudzio safety-net diagnostic, output:\n{log}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn rudzio_runner_sets_full_backtrace_when_unset(_ctx: &Test) -> anyhow::Result<()> {
        // Run the bg-panic fixture WITHOUT pre-setting RUST_BACKTRACE
        // and assert the panic output carries a frame from the user's
        // panic site (the `panic!` line in the fixture). The "full"
        // backtrace mode is what produces frame-by-frame output that
        // includes user code; "short" mode would clip everything below
        // `__rust_end_short_backtrace` (which is where the rudzio entry
        // sits, i.e. the user frame would be hidden).
        use std::process::Command;
        let out = Command::new(env!("CARGO_BIN_EXE_bg_thread_panic_in_setup_tokio_mt"))
            .args(["--plain"])
            .env_remove("RUST_BACKTRACE")
            .env_remove("RUST_LIB_BACKTRACE")
            .output()?;
        let log = log_of(&out);
        anyhow::ensure!(
            log.contains("bg_thread_panic_in_setup_tokio_mt.rs"),
            "full backtrace must include the user's panic site, output:\n{log}"
        );
        Ok(())
    }

    /// L1.1. Layer 1 process-exit watchdog. With Layer 2 disabled
    /// (`--phase-hang-grace=0`) and a sync-blocking body, neither the
    /// per-test wrapper nor the run-timeout's cooperative cancel can
    /// unblock the worker. The watchdog observes the root token
    /// cancelled, sleeps `--cancel-grace-period=1`, then
    /// `process::exit(2)`.
    ///
    /// **Deterministic shape:** the correctness signal is the
    /// process exit code (2) and the log marker (`force-exiting`).
    /// No wall-clock bound is asserted — `run_serial_with_args`
    /// blocks on the subprocess exit, so the test naturally takes
    /// run-timeout + cancel-grace seconds + scheduling overhead.
    /// Under saturated CPU contention that's slower, but the
    /// outcome stays identical.
    #[rudzio::test]
    fn cancel_grace_force_exits_with_code_2(_ctx: &Test) -> anyhow::Result<()> {
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_hung_run_grace_tokio_mt"),
            &[
                "--run-timeout=1",
                "--cancel-grace-period=1",
                "--phase-hang-grace=0",
            ],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(2_i32),
            "expected exit code 2 (force-exit), got {:?}; output:\n{log}",
            out.status.code()
        );
        anyhow::ensure!(
            log.contains("force-exiting"),
            "expected 'force-exiting' diagnostic on stderr, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("body_sync_blocks_60s_unreached_marker"),
            "sync-blocked body must not have completed, output:\n{log}"
        );
        Ok(())
    }

    /// L1.1b. Layer 1 watchdog OFF — `--cancel-grace-period=0` opts
    /// out of the safety net entirely. We can't actually let the
    /// fixture run forever in a unit test, so we set
    /// `--run-timeout=1` and assert that without the watchdog the
    /// fixture takes longer to exit than the watchdog-on case.
    /// Specifically: with the watchdog OFF, the sync-blocking
    /// `spawn_blocking` task's `JoinHandle` await keeps the dispatch
    /// loop alive until the OS sleep finishes (or until tokio's
    /// runtime drop forces the blocking pool to wind down) — much
    /// longer than 2s. We bound the wait at 8s and assert exit is
    /// NOT code 2 (since the watchdog didn't fire).
    ///
    /// This serves as the inverse-direction guard: confirms users who
    /// explicitly disable the watchdog don't get a surprise
    /// `process::exit(2)`. Skipped if the runtime would actually
    /// hang past the harness budget.
    /// With `--cancel-grace-period=0`, the parser encodes the watchdog
    /// as `None`, so it's never spawned. The subprocess body
    /// `spawn_blocking(sleep(60s))` can't be cancelled cooperatively,
    /// and tokio's runtime drop waits for the blocking pool — so the
    /// subprocess hangs forever by design. The correctness signal is
    /// pure absence: if the watchdog ever fires, it does so at
    /// `t = run_timeout + grace = 1s + 0s = 1s` sharp with exit code
    /// 2. We give it 3s (2s margin) then kill the child and conclude
    /// the watchdog is correctly off.
    #[rudzio::test]
    fn cancel_grace_zero_does_not_force_exit(_ctx: &Test) -> anyhow::Result<()> {
        // Use a 1-thread tokio runtime so the body's spawn_blocking
        // sleep can't be exited cooperatively.
        let mut child = Command::new(env!("CARGO_BIN_EXE_hung_run_grace_tokio_mt"))
            .env("NO_COLOR", "1")
            .env("RUST_TEST_THREADS", "1")
            .args(["--run-timeout=1", "--cancel-grace-period=0"])
            .spawn()?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(3_u64))
            .ok_or_else(|| anyhow::anyhow!("deadline overflow"))?;
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(
                    status.code() != Some(2_i32),
                    "with watchdog disabled, exit code must NOT be 2 (force-exit); got {:?}",
                    status.code()
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _killed: IoResult<()> = child.kill();
                let _waited: IoResult<ExitStatus> = child.wait();
                // Watchdog is off (would have fired at t=1s if on);
                // hang past the deadline is the expected outcome.
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50_u64));
        }
    }

    #[rudzio::test]
    fn rudzio_runner_respects_user_backtrace_choice(_ctx: &Test) -> anyhow::Result<()> {
        // When the user explicitly disables backtrace, the runner must
        // not silently override it. Pass `RUST_BACKTRACE=0` and
        // verify the backtrace frames are absent. The panic line
        // itself still prints (default hook always emits it).
        use std::process::Command;
        let out = Command::new(env!("CARGO_BIN_EXE_bg_thread_panic_in_setup_tokio_mt"))
            .args(["--plain"])
            .env("RUST_BACKTRACE", "0")
            .env("RUST_LIB_BACKTRACE", "0")
            .output()?;
        let log = log_of(&out);
        anyhow::ensure!(
            log.contains("simulated_bg_thread_panic_during_setup"),
            "panic line must still print, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("stack backtrace:"),
            "user's RUST_BACKTRACE=0 must be respected, got backtrace anyway:\n{log}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn gradual_cancellation_waits_for_tracked_tasks(_ctx: &Test) -> anyhow::Result<()> {
        // The tracked background task prints its cleanup marker only when the
        // runner's suite teardown drains the TaskTracker after root cancel.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_gradual_cancel_tokio_mt"),
            &["--run-timeout=1"],
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("gradual_cancel_cleanup_marker"),
            "cleanup marker missing — runner did not wait for graceful cancel, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 1,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["task_cleans_up_on_cancel"])?;
        Ok(())
    }

    #[rudzio::test]
    fn multi_file_suites_share_one_main_runner(_ctx: &Test) -> anyhow::Result<()> {
        // Tokens registered by separate source files must all show up in a
        // single run under a single `rudzio::run()` call.
        let out = run(env!("CARGO_BIN_EXE_multi_file_suite"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "expected exit 0, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 4,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 4,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &[
                "module_a::module_a_first",
                "module_a::module_a_second",
                "module_b::module_b_first",
                "module_b::module_b_second",
            ],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn serial_runner_does_not_execute_tests_concurrently(_ctx: &Test) -> anyhow::Result<()> {
        // With RUST_TEST_THREADS=1 the barrier fixture must fail:
        // the first test hits the barrier and times out before the others arrive.
        let out = run_serial(env!("CARGO_BIN_EXE_parallel_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 under serial dispatch, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("barrier timed out"),
            "expected a barrier-timeout failure, output:\n{log}"
        );
        Ok(())
    }
}

#[cfg(unix)]
#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod unix_tests {
    use std::env::temp_dir;
    use std::fs;
    use std::path::PathBuf;
    use std::process::id as process_id;
    use std::time::SystemTime;

    use anyhow::Context as _;

    use super::{
        Command, Duration, ExitStatus, Instant, IoResult, LogPathGuard, RunSummary, Stdio, Test,
        assert_exact_test_names, assert_summary, log_of, ordered, run, run_and_signal, thread,
    };

    use rustix::process::{Pid, Signal, kill_process};

    #[rudzio::test]
    fn sigint_cancels_run_gracefully(_ctx: &Test) -> anyhow::Result<()> {
        let out = run_and_signal(
            env!("CARGO_BIN_EXE_sigint_cancel_tokio_mt"),
            "sigint_cancel_ready_marker",
            Signal::INT,
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 after SIGINT cancellation, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("received SIGINT"),
            "expected the runner's signal diagnostic, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("sigint_cancel_observed_marker"),
            "test body must see cancellation via its context token, output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("never_runs_after_sigint_unreached_marker"),
            "queued test must not start after SIGINT, output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 1,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["waits_for_sigint", "never_runs_after_sigint"])?;
        Ok(())
    }

    #[rudzio::test]
    fn stdio_is_attributed_to_the_originating_test(_ctx: &Test) -> anyhow::Result<()> {
        // Plain mode (when stdout is a pipe, as in this integration
        // test) keeps each test's stdout/stderr on its own stream
        // without merging. Attribution is provable via ordering:
        // alpha/beta/gamma run serially, so within each stream the
        // per-test lines must appear in that order, each test's
        // stdout lines bracketed by its own lifecycle markers on the
        // stdout stream, and its stderr lines bracketed by the
        // neighbouring tests' stderr markers. The panic payload goes
        // to stderr — must appear AFTER gamma's stderr markers and
        // proves the runner routed the panic through the owning
        // test's stream.
        let out = run(env!("CARGO_BIN_EXE_stdio_attribution_tokio_mt"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 (one panic)\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );
        assert_summary(
            &stdout,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 1,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&stdout, &["alpha", "beta", "gamma_panics"])?;

        // stdout stream: each test's stdout lines appear between its
        // own lifecycle start and the next test's lifecycle start,
        // in the order alpha < beta < gamma. Test display names go
        // through `normalize_module_path` which strips the crate name
        // and any `tests` segment auto-generated by the suite macro,
        // so the rendered names are bare (`alpha`, `setup`, …) for
        // single-binary fixtures like this one.
        ordered(
            &stdout,
            &[
                "[OK]      setup",
                "alpha_stdout_line_1",
                "alpha_stdout_line_2",
                "alpha_stdout_line_3",
                "[OK]      alpha",
                "beta_stdout_line_1",
                "beta_stdout_line_2",
                "beta_stdout_line_3",
                "[OK]      beta",
                "gamma_stdout_line_1",
                "gamma_stdout_line_2",
                "gamma_stdout_line_3",
                "[PANIC]   gamma_panics",
            ],
        )?;

        // stderr stream: each test's stderr lines appear in the
        // same order; panic payload comes after gamma's stderr
        // markers (proving the runner's panic-capture attributed
        // the unwind message to the owning test's stderr, not to
        // someone else's block or a floating process-wide stream).
        ordered(
            &stderr,
            &[
                "alpha_stderr_line_1",
                "alpha_stderr_line_2",
                "alpha_stderr_line_3",
                "beta_stderr_line_1",
                "beta_stderr_line_2",
                "beta_stderr_line_3",
                "gamma_stderr_line_1",
                "gamma_stderr_line_2",
                "gamma_stderr_line_3",
                "gamma_panic_message_line_1",
                "gamma_panic_message_line_2",
            ],
        )?;

        // Cross-stream hygiene: stdout markers never appear on
        // stderr and stderr markers never appear on stdout. If the
        // runner ever starts routing stdio through a shared channel,
        // this guard triggers.
        for marker in [
            "alpha_stderr_line_1",
            "beta_stderr_line_1",
            "gamma_stderr_line_1",
            "gamma_panic_message_line_1",
        ] {
            anyhow::ensure!(
                !stdout.contains(marker),
                "stderr marker `{marker}` leaked into stdout:\n{stdout}",
            );
        }
        for marker in [
            "alpha_stdout_line_1",
            "beta_stdout_line_1",
            "gamma_stdout_line_1",
        ] {
            anyhow::ensure!(
                !stderr.contains(marker),
                "stdout marker `{marker}` leaked into stderr:\n{stderr}",
            );
        }

        Ok(())
    }

    #[rudzio::test]
    fn teardown_always_runs_and_trackers_drain(_ctx: &Test) -> anyhow::Result<()> {
        // Proves two invariants in one fixture:
        //   1. Test::teardown is called exactly once per test, across
        //      the full outcome matrix we exercise here (pass, fail,
        //      panic).
        //   2. Both the per-test and suite TaskTracker drain — every
        //      spawned tracked task is observed as completed by the
        //      matching teardown before it returns.
        // The fixture's Suite::teardown prints one INVARIANTS_CHECK
        // line with the eight counters; we parse it and assert.
        let out = run(env!("CARGO_BIN_EXE_teardown_invariants_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 (fail + panic by design), output:\n{log}",
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 1,
                panicked: 1,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 3,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["passing", "failing", "panicking"])?;
        let line = log
            .lines()
            .find(|line| line.contains("INVARIANTS_CHECK"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "INVARIANTS_CHECK line missing — Suite::teardown never ran, output:\n{log}"
                )
            })?;
        let expected = "INVARIANTS_CHECK suite_setup=1 suite_teardown=1 test_setup=3 test_teardown=3 test_tasks_spawned=3 test_tasks_completed=3 suite_tasks_spawned=1 suite_tasks_completed=1";
        anyhow::ensure!(
            line.contains(expected),
            "invariants mismatch\n  expected: {expected}\n  got: {line}\n  full output:\n{log}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn two_suites_same_tuple_collapse_into_one_group(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_group_dedup_tokio_mt"))?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0_i32),
            "fixture must exit 0 (both tests pass), output:\n{log}"
        );
        let setup_lines = log.matches("COUNTING_SUITE_SETUP").count();
        let teardown_lines = log.matches("COUNTING_SUITE_TEARDOWN").count();
        anyhow::ensure!(
            setup_lines == 1,
            "expected exactly 1 Suite::setup invocation across both \
             `#[rudzio::suite]` blocks sharing the same (runtime, \
             suite, test) tuple; counted {setup_lines}, output:\n{log}",
        );
        anyhow::ensure!(
            teardown_lines == 1,
            "expected exactly 1 Suite::teardown invocation; counted \
             {teardown_lines}, output:\n{log}",
        );
        anyhow::ensure!(
            log.contains("in_first_mod") && log.contains("in_second_mod"),
            "both tests must have run under the collapsed group, output:\n{log}",
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 2,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 0,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(
            &log,
            &["in_first_mod", "in_second_mod"],
        )?;
        Ok(())
    }

    #[rudzio::test]
    fn sigterm_cancels_run_gracefully(_ctx: &Test) -> anyhow::Result<()> {
        // Same fixture, delivered SIGTERM instead of SIGINT, to prove the
        // `termination` feature of ctrlc is actually wired through.
        let out = run_and_signal(
            env!("CARGO_BIN_EXE_sigint_cancel_tokio_mt"),
            "sigint_cancel_ready_marker",
            Signal::TERM,
        )?;
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1_i32),
            "expected exit 1 after SIGTERM cancellation, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("received SIGTERM"),
            "expected the runner's signal diagnostic, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("sigint_cancel_observed_marker"),
            "output:\n{log}"
        );
        anyhow::ensure!(
            !log.contains("never_runs_after_sigint_unreached_marker"),
            "output:\n{log}"
        );
        assert_summary(
            &log,
            RunSummary {
                passed: 1,
                failed: 0,
                panicked: 0,
                timed_out: 0,
                cancelled: 1,
                ignored: 0,
                teardown_failed: 0,
                measured: 0,
                total: 2,
                filtered_out: 0,
            },
        )?;
        assert_exact_test_names(&log, &["waits_for_sigint", "never_runs_after_sigint"])?;
        Ok(())
    }

    /// **Bug #13** regression: `CaptureGuard::drop` must NOT block
    /// indefinitely when a child process inherited the pipe write
    /// ends. The fixture initialises capture, spawns `sleep 600`
    /// (which inherits FDs 1 and 2 — the pipe write ends after
    /// `output::init` — by default `Stdio::inherit()`), then drops
    /// the guard.
    ///
    /// Pre-fix: drop joins the reader threads. Readers only exit on
    /// `read() == 0`, which the kernel reports only when every
    /// write-end FD is closed; the child holds one, so the reader
    /// blocks forever and the process never exits.
    ///
    /// Post-fix: drop completes within a small bound. The fixture
    /// reaches its post-drop log write ("`rudzio_capture_drop_returned_in_ms=…`")
    /// and exits 0; we read the log, assert the marker, and SIGKILL
    /// the orphaned `sleep` child via its harvested pid. The
    /// fixture's stdio is wired to `Stdio::null()` so the orphaned
    /// `sleep` child can't drag this test process into a secondary
    /// FD-inheritance hang.
    #[rudzio::test]
    fn capture_guard_drop_returns_even_when_child_holds_pipe_fds(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        use std::os::unix::process::ExitStatusExt as _;

        // Manual temp path — fixtures crate doesn't pull in tempfile.
        // Uses pid + nanos so concurrent test instances per runtime
        // don't collide.
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0_u128, |dur| dur.as_nanos());
        let log_path: PathBuf = temp_dir().join(format!(
            "rudzio-bug13-{pid}-{nanos}.log",
            pid = process_id(),
        ));
        let _log_guard = LogPathGuard {
            path: log_path.clone(),
        };

        let mut child = Command::new(env!(
            "CARGO_BIN_EXE_capture_guard_drop_with_child_holding_fds"
        ))
        .env("RUDZIO_CAPTURE_TEST_LOG", &log_path)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn capture_guard_drop_with_child_holding_fds")?;

        // Poll for exit with a 5s deadline. Pre-fix the fixture hangs
        // forever at drop; we kill it, kill the orphaned sleep child,
        // and surface a clear failure.
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(5_u64))
            .ok_or_else(|| anyhow::anyhow!("deadline overflow"))?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _killed: IoResult<()> = child.kill();
                let waited = child.wait()?;
                // Best-effort orphan cleanup from the log file even on
                // the failure path.
                if let Ok(log) = fs::read_to_string(&log_path) {
                    kill_orphaned_child_by_pid_line(&log);
                }
                anyhow::bail!(
                    "CaptureGuard::drop hung — fixture did not exit within 5s. \
                     This is the Bug #13 root cause (child holds inherited \
                     pipe FDs, reader.join() blocks forever). Final status: \
                     {:?}",
                    waited.code()
                );
            }
            thread::sleep(Duration::from_millis(50_u64));
        };

        let log = fs::read_to_string(&log_path).context("read fixture log")?;
        kill_orphaned_child_by_pid_line(&log);

        let status_raw = ExitStatus::from_raw(status.into_raw());
        anyhow::ensure!(
            status_raw.code() == Some(0_i32),
            "expected fixture to exit 0 after dropping CaptureGuard; got {:?}.\n--- log ---\n{log}",
            status_raw.code()
        );
        anyhow::ensure!(
            log.contains("rudzio_capture_drop_returned_in_ms="),
            "fixture must write the post-drop marker to the log, content:\n{log}",
        );
        Ok(())
    }

    /// SIGKILL the orphaned `sleep` child the fixture left behind.
    /// The fixture writes a `rudzio_capture_test_child_pid=<pid>`
    /// line to its log; we harvest the pid and deliver SIGKILL.
    /// Best-effort — silently no-ops when the line is absent or the
    /// pid is already gone.
    fn kill_orphaned_child_by_pid_line(log: &str) {
        if let Some(line) = log
            .lines()
            .find(|line| line.starts_with("rudzio_capture_test_child_pid="))
            && let Some(pid_str) = line.strip_prefix("rudzio_capture_test_child_pid=")
            && let Ok(pid) = pid_str.trim().parse::<i32>()
        {
            // SIGKILL the inherited child; pid is a one-shot value harvested
            // from the fixture's own log. `rustix` wraps `kill(2)` safely.
            if let Some(child_pid) = Pid::from_raw(pid) {
                let _killed = kill_process(child_pid, Signal::KILL);
            }
        }
    }
}
