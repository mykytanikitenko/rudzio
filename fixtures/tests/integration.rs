//! Rudzio tests itself: this file runs under `#[rudzio::main]` (via
//! `harness = false`) and every child-process assertion lives inside a
//! `#[rudzio::suite]` module driven by a tokio multi-thread runtime.

use std::io::Read as _;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

fn run(exe: &str) -> Output {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {exe}: {e}"))
}

fn run_serial_with_args(exe: &str, args: &[&str]) -> Output {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", "1")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {exe}: {e}"))
}

fn run_serial(exe: &str) -> Output {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", "1")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {exe}: {e}"))
}

fn run_parallel(exe: &str, threads: u32) -> Output {
    Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", threads.to_string())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {exe}: {e}"))
}

/// Spawn the fixture, wait for a readiness marker on its stdout, then send
/// the given Unix signal. Returns the combined stdout+stderr of the child.
#[cfg(unix)]
fn run_and_signal(exe: &str, ready_marker: &str, signal: i32) -> Output {
    use std::os::unix::process::ExitStatusExt as _;

    let mut child = Command::new(exe)
        .env("NO_COLOR", "1")
        .env("RUST_TEST_THREADS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {exe}: {e}"));

    // Read stdout incrementally until we see the readiness marker.
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut stdout_buf = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut tmp = [0u8; 256];
    while !String::from_utf8_lossy(&stdout_buf).contains(ready_marker) {
        if std::time::Instant::now() >= deadline {
            let _killed = child.kill();
            let _waited = child.wait();
            panic!("readiness marker {ready_marker:?} never appeared");
        }
        match stdout.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => stdout_buf.extend_from_slice(&tmp[..n]),
            Err(e) => panic!("read child stdout: {e}"),
        }
    }

    // Give the runner a short grace period to install its signal handler.
    thread::sleep(Duration::from_millis(100));

    #[allow(unsafe_code, reason = "integration test helper delivering a signal")]
    // SAFETY: `kill(2)` is signal-safe and the child pid is valid.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    assert_eq!(rc, 0, "kill({signal}) failed");

    // Drain the rest of stdout concurrently with stderr.
    let mut stderr = child.stderr.take().expect("child stderr");
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _read = stderr.read_to_end(&mut buf);
        buf
    });
    let _read = stdout.read_to_end(&mut stdout_buf);
    let stderr_buf = stderr_handle.join().expect("stderr thread");

    let status = child.wait().expect("wait child");
    Output {
        status: std::process::ExitStatus::from_raw(status.into_raw()),
        stdout: stdout_buf,
        stderr: stderr_buf,
    }
}

fn log_of(out: &Output) -> String {
    // Combine stdout and stderr; framework output goes to both.
    let mut buf = String::from_utf8_lossy(&out.stdout).into_owned();
    buf.push_str(&String::from_utf8_lossy(&out.stderr));
    buf
}

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use super::*;
    use rudzio::common::context::Test;

    #[rudzio::test]
    fn mut_test_context_is_borrowable(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_mutable_test_context"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("3 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 failed"), "output:\n{log}");
        anyhow::ensure!(log.contains("mutates_via_mut_borrow"), "output:\n{log}");
        anyhow::ensure!(
            log.contains("sync_mutates_via_mut_borrow"),
            "output:\n{log}"
        );
        anyhow::ensure!(log.contains("fresh_ctx_per_test"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn passing_tokio_mt_succeeds(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("first_passes"), "output:\n{log}");
        anyhow::ensure!(log.contains("second_passes"), "output:\n{log}");
        anyhow::ensure!(log.contains("[OK]"), "output:\n{log}");
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn failing_tokio_mt_exits_one(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_failing_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[FAIL]"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn ignored_tests_are_skipped(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_ignored_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[IGNORE]"), "output:\n{log}");
        anyhow::ensure!(log.contains("takes too long"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("2 ignored"), "output:\n{log}");
        anyhow::ensure!(!log.contains("must not run"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn tokio_current_thread_runtime_works(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_tokio_ct"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("yields_then_passes"), "output:\n{log}");
        anyhow::ensure!(log.contains("[OK]"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn compio_runtime_works(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_compio"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("passes_under_compio"), "output:\n{log}");
        anyhow::ensure!(log.contains("[OK]"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn futures_runtime_works(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_passing_futures"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("passes_under_futures"), "output:\n{log}");
        anyhow::ensure!(log.contains("spawn_works_under_futures"), "output:\n{log}");
        anyhow::ensure!(log.contains("futures::ThreadPool"), "output:\n{log}");
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn multi_runtime_runs_every_config(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_multi_runtime"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("Multithread"), "output:\n{log}");
        anyhow::ensure!(log.contains("compio::Runtime"), "output:\n{log}");
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn panicking_test_does_not_halt_the_suite(_ctx: &Test) -> anyhow::Result<()> {
        // Expected behavior: a panic in one test is isolated — subsequent tests
        // in the same runtime group still execute and the summary reports
        // 2 passed, 1 panicked, 3 total.
        let out = run(env!("CARGO_BIN_EXE_panics_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(log.contains("before_panic"), "output:\n{log}");
        anyhow::ensure!(
            log.contains("after_panic"),
            "after_panic never ran — panic killed the whole thread. output:\n{log}"
        );
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 panicked"), "output:\n{log}");
        anyhow::ensure!(log.contains("3 total"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn sync_tests_mix_pass_fail_panic(_ctx: &Test) -> anyhow::Result<()> {
        // All three sync tests run; isolation keeps panics from killing the thread.
        let out = run(env!("CARGO_BIN_EXE_sync_mixed_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("sync_passes"), "output:\n{log}");
        anyhow::ensure!(log.contains("sync_returns_err"), "output:\n{log}");
        anyhow::ensure!(log.contains("sync_panics"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 failed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 panicked"), "output:\n{log}");
        anyhow::ensure!(log.contains("3 total"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn custom_suite_and_test_impls_work(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_custom_context_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("runs_on_custom_context"), "output:\n{log}");
        anyhow::ensure!(log.contains("[OK]"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_failure_aborts_the_runtime_group(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_setup_failure_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
            "expected exit 1, output:\n{log}"
        );
        // The new lifecycle line + the error's Display must both appear.
        anyhow::ensure!(log.contains("[FAIL]"), "output:\n{log}");
        anyhow::ensure!(log.contains("setup "), "output:\n{log}");
        anyhow::ensure!(log.contains("setup_failed_by_design"), "output:\n{log}");
        // Tests that never ran are reported as Cancelled, not Panicked.
        anyhow::ensure!(log.contains("1 cancelled"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 panicked"), "output:\n{log}");
        // No test emits an [OK] tag; setup failed before teardown ran, so
        // the [OK] teardown line never gets emitted either.
        anyhow::ensure!(!log.contains("[OK]"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn context_creation_failure_counts_as_failed(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_context_creation_failure_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("2 failed"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 panicked"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_and_teardown_lines_appear_in_passing_run(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        // A normal passing run must emit visible setup/teardown
        // lifecycle lines so the user knows the suite phases happened
        // (the whole point of the new output: see *that* it's
        // happening, not just *whether* it failed).
        let out = run(env!("CARGO_BIN_EXE_passing_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
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
        Ok(())
    }

    #[rudzio::test]
    fn suite_teardown_failure_is_reported(_ctx: &Test) -> anyhow::Result<()> {
        // Teardown errors used to print as a one-line `warning:` and
        // were easy to miss. Now they emit a `[FAIL] teardown` line
        // carrying the error message and contribute to the
        // teardown_failures count, which drives the run's exit code.
        let out = run(env!("CARGO_BIN_EXE_teardown_failure_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("body_runs_then_teardown_fails"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 teardown failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        // catch_unwind wrapper around Suite::setup turns the panic
        // into a structured `[FAIL] setup` line carrying the panic
        // message instead of unwinding through the runtime thread
        // (which would print the generic "runtime thread panicked").
        let out = run(env!("CARGO_BIN_EXE_panic_in_suite_setup_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 cancelled"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn suite_teardown_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_panic_in_suite_teardown_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 teardown failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn test_setup_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        // catch_unwind around `Suite::context` turns a per-test setup
        // panic into a `TestOutcome::SetupFailed` carrying the panic
        // message — rendered with the `[SETUP]` status tag.
        let out = run(env!("CARGO_BIN_EXE_panic_in_test_setup_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn test_teardown_panic_is_caught_and_reported(_ctx: &Test) -> anyhow::Result<()> {
        // catch_unwind around `Test::teardown` routes the panic
        // through the structured `report_test_teardown_failure`
        // method (no `report_warning` escape hatch), bumps the
        // per-test teardown counter, and the run exits non-zero.
        let out = run(env!("CARGO_BIN_EXE_panic_in_test_teardown_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 teardown failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn plain_test_attribute_is_recognized(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_plain_test_attr_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("runs_via_plain_test_attribute"),
            "output:\n{log}"
        );
        anyhow::ensure!(log.contains("[OK]"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn bare_return_types_compile_and_pass(_ctx: &Test) -> anyhow::Result<()> {
        // Captures the promise that `#[rudzio::test]` handles every
        // return shape `#[test]` / `#[tokio::test]` accept — users
        // don't have to rewrite void bodies into `-> anyhow::Result`.
        // If `rudzio::IntoRudzioResult` loses an impl, this fixture
        // stops compiling; regressions surface at the PR level.
        let out = run(env!("CARGO_BIN_EXE_bare_return_types_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        for name in [
            "bare_sync_void",
            "explicit_unit_return",
            "bare_async_void",
            "result_returning",
            "result_display_only",
        ] {
            anyhow::ensure!(
                log.contains(name),
                "fixture must report test `{name}`, output:\n{log}"
            );
        }
        anyhow::ensure!(log.contains("5 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn multiple_panics_are_isolated_and_ordered(_ctx: &Test) -> anyhow::Result<()> {
        // RUST_TEST_THREADS=1 forces strict serial execution so the
        // source-order assertion below is meaningful.
        let out = run_serial(env!("CARGO_BIN_EXE_multi_panic_ordering_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(log.contains("3 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("2 panicked"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 failed"), "output:\n{log}");
        anyhow::ensure!(log.contains("5 total"), "output:\n{log}");

        // Sequential source-order execution: each test name must come before the next.
        let positions: Vec<_> = [
            "step_1_pass",
            "step_2_panic",
            "step_3_pass",
            "step_4_panic",
            "step_5_pass",
        ]
        .iter()
        .map(|name| {
            log.find(name)
                .unwrap_or_else(|| panic!("missing {name} in output:\n{log}"))
        })
        .collect();
        anyhow::ensure!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "tests ran out of order; positions {positions:?}, output:\n{log}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn tracker_drains_on_suite_teardown(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_tracker_drain_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[OK]"), "output:\n{log}");
        // The marker only appears if suite teardown waited for the tracked task.
        anyhow::ensure!(
            log.contains("tracker_drain_marker"),
            "tracker did not drain the spawned task; output:\n{log}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn per_test_teardown_cancels_the_cancel_token(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_cancel_token_propagation_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("[OK]"), "output:\n{log}");
        anyhow::ensure!(
            log.contains("cancel_propagation_marker"),
            "teardown did not cancel the token; output:\n{log}"
        );
        Ok(())
    }

    #[rudzio::test]
    fn spawn_tracked_test_passes(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_spawn_tracked"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("spawn_awaits_result"), "output:\n{log}");
        anyhow::ensure!(log.contains("cancel_token_is_child"), "output:\n{log}");
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn sync_work_runs_on_the_runtimes_blocking_pool(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_spawn_blocking_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
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
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn parallel_runner_executes_tests_concurrently(_ctx: &Test) -> anyhow::Result<()> {
        // The fixture has three tests that synchronise on a Barrier::new(3) with a 2s timeout.
        // If the runner dispatches all three concurrently, the barrier releases and every test passes.
        let out = run_parallel(env!("CARGO_BIN_EXE_parallel_tokio_mt"), 3);
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0 under parallel dispatch, output:\n{log}"
        );
        anyhow::ensure!(log.contains("3 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn multiple_runtime_groups_run_in_separate_threads_concurrently(
        _ctx: &Test,
    ) -> anyhow::Result<()> {
        // Fixture has two runtime groups (tokio multi-thread + compio), each with one test.
        // Both tests block on a shared Barrier(2) via spawn_blocking. If the groups' threads
        // run in parallel, both arrive at the barrier and it releases.
        let out = run(env!("CARGO_BIN_EXE_cross_runtime_parallel"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0 (watchdog exit 2 means groups serialised), output:\n{log}"
        );
        anyhow::ensure!(log.contains("Multithread"), "output:\n{log}");
        anyhow::ensure!(log.contains("compio::Runtime"), "output:\n{log}");
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
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
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        Ok(())
    }

    #[rudzio::test]
    fn per_test_timeout_cancels_only_the_offending_test(_ctx: &Test) -> anyhow::Result<()> {
        // With `--test-timeout=1`, the per-test watchdog must fire only on the
        // hanging test; the other test in the same suite must still run.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_timeout_tokio_mt"),
            &["--test-timeout=1"],
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 timed out"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn run_timeout_cancels_the_whole_run(_ctx: &Test) -> anyhow::Result<()> {
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_run_timeout_tokio_mt"),
            &["--run-timeout=1"],
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("2 cancelled"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn suite_setup_timeout_cancels_every_test(_ctx: &Test) -> anyhow::Result<()> {
        // `--suite-setup-timeout=1` fires while suite setup hangs; every
        // queued test reports as cancelled and the binary exits non-zero.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_suite_setup_timeout_tokio_mt"),
            &["--suite-setup-timeout=1"],
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 teardown failed"), "output:\n{log}");
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
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 teardown failed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn per_test_attr_body_timeout_overrides_default(_ctx: &Test) -> anyhow::Result<()> {
        // No CLI timeouts. The annotated test (`#[rudzio::test(timeout = 1)]`)
        // times out via attribute alone; sibling test stays unbounded.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_attr_body_timeout_tokio_mt"),
            &[],
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 timed out"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn per_test_attr_setup_timeout_overrides_default(_ctx: &Test) -> anyhow::Result<()> {
        // No CLI timeouts. Setup phase times out only because of the
        // attribute override on this test.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_attr_setup_timeout_tokio_mt"),
            &[],
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        Ok(())
    }

    #[rudzio::test]
    fn per_test_attr_teardown_timeout_overrides_default(_ctx: &Test) -> anyhow::Result<()> {
        // No CLI timeouts. Teardown phase times out only because of the
        // attribute override on this test. Body still passes.
        let out = run_serial_with_args(
            env!("CARGO_BIN_EXE_per_test_attr_teardown_timeout_tokio_mt"),
            &[],
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
            "expected exit 1, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("[TIMEOUT]") && log.contains("teardown"),
            "expected [TIMEOUT] teardown line, output:\n{log}"
        );
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("1 teardown failed"), "output:\n{log}");
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
        let out = run_serial(env!("CARGO_BIN_EXE_bg_thread_panic_in_setup_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(
            log.contains("gradual_cancel_cleanup_marker"),
            "cleanup marker missing — runner did not wait for graceful cancel, output:\n{log}"
        );
        anyhow::ensure!(log.contains("1 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn multi_file_suites_share_one_main_runner(_ctx: &Test) -> anyhow::Result<()> {
        // Tokens registered by separate source files must all show up in a
        // single run under a single `rudzio::run()` call.
        let out = run(env!("CARGO_BIN_EXE_multi_file_suite"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
            "expected exit 0, output:\n{log}"
        );
        anyhow::ensure!(log.contains("module_a_first"), "output:\n{log}");
        anyhow::ensure!(log.contains("module_a_second"), "output:\n{log}");
        anyhow::ensure!(log.contains("module_b_first"), "output:\n{log}");
        anyhow::ensure!(log.contains("module_b_second"), "output:\n{log}");
        anyhow::ensure!(log.contains("4 passed"), "output:\n{log}");
        anyhow::ensure!(log.contains("0 failed"), "output:\n{log}");
        anyhow::ensure!(log.contains("4 total"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn serial_runner_does_not_execute_tests_concurrently(_ctx: &Test) -> anyhow::Result<()> {
        // With RUST_TEST_THREADS=1 the barrier fixture must fail:
        // the first test hits the barrier and times out before the others arrive.
        let out = run_serial(env!("CARGO_BIN_EXE_parallel_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod unix_tests {
    use super::*;
    use rudzio::common::context::Test;

    #[rudzio::test]
    fn sigint_cancels_run_gracefully(_ctx: &Test) -> anyhow::Result<()> {
        let out = run_and_signal(
            env!("CARGO_BIN_EXE_sigint_cancel_tokio_mt"),
            "sigint_cancel_ready_marker",
            libc::SIGINT,
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 cancelled"), "output:\n{log}");
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
        let out = run(env!("CARGO_BIN_EXE_stdio_attribution_tokio_mt"));
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        anyhow::ensure!(
            out.status.code() == Some(1),
            "expected exit 1 (one panic)\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );

        fn index_of<'a>(hay: &'a str, needle: &str) -> anyhow::Result<usize> {
            hay.find(needle)
                .ok_or_else(|| anyhow::anyhow!("`{needle}` missing"))
        }
        fn ordered<'a>(hay: &'a str, needles: &[&str]) -> anyhow::Result<()> {
            let mut last = 0_usize;
            for n in needles {
                let pos = index_of(hay, n).map_err(|e| {
                    anyhow::anyhow!("{e} in stream:\n{hay}")
                })?;
                anyhow::ensure!(
                    pos >= last,
                    "`{n}` found at {pos} but expected at >= {last} (previous neighbour). Stream:\n{hay}",
                );
                last = pos;
            }
            Ok(())
        }

        // stdout stream: each test's stdout lines appear between its
        // own lifecycle start and the next test's lifecycle start,
        // in the order alpha < beta < gamma.
        ordered(
            &stdout,
            &[
                "[OK]      setup stdio_attribution_tokio_mt::tests",
                "alpha_stdout_line_1",
                "alpha_stdout_line_2",
                "alpha_stdout_line_3",
                "[OK]      stdio_attribution_tokio_mt::alpha",
                "beta_stdout_line_1",
                "beta_stdout_line_2",
                "beta_stdout_line_3",
                "[OK]      stdio_attribution_tokio_mt::beta",
                "gamma_stdout_line_1",
                "gamma_stdout_line_2",
                "gamma_stdout_line_3",
                "[PANIC]   stdio_attribution_tokio_mt::gamma_panics",
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
        for s in [
            "alpha_stderr_line_1",
            "beta_stderr_line_1",
            "gamma_stderr_line_1",
            "gamma_panic_message_line_1",
        ] {
            anyhow::ensure!(
                !stdout.contains(s),
                "stderr marker `{s}` leaked into stdout:\n{stdout}",
            );
        }
        for s in [
            "alpha_stdout_line_1",
            "beta_stdout_line_1",
            "gamma_stdout_line_1",
        ] {
            anyhow::ensure!(
                !stderr.contains(s),
                "stdout marker `{s}` leaked into stderr:\n{stderr}",
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
        let out = run(env!("CARGO_BIN_EXE_teardown_invariants_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
            "expected exit 1 (fail + panic by design), output:\n{log}",
        );
        let line = log
            .lines()
            .find(|l| l.contains("INVARIANTS_CHECK"))
            .ok_or_else(|| anyhow::anyhow!(
                "INVARIANTS_CHECK line missing — Suite::teardown never ran, output:\n{log}"
            ))?;
        let expected = "INVARIANTS_CHECK suite_setup=1 suite_teardown=1 test_setup=3 test_teardown=3 test_tasks_spawned=3 test_tasks_completed=3 suite_tasks_spawned=1 suite_tasks_completed=1";
        anyhow::ensure!(
            line.contains(expected),
            "invariants mismatch\n  expected: {expected}\n  got: {line}\n  full output:\n{log}",
        );
        Ok(())
    }

    #[rudzio::test]
    fn two_suites_same_tuple_collapse_into_one_group(_ctx: &Test) -> anyhow::Result<()> {
        let out = run(env!("CARGO_BIN_EXE_group_dedup_tokio_mt"));
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(0),
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
        anyhow::ensure!(log.contains("2 passed"), "output:\n{log}");
        Ok(())
    }

    #[rudzio::test]
    fn sigterm_cancels_run_gracefully(_ctx: &Test) -> anyhow::Result<()> {
        // Same fixture, delivered SIGTERM instead of SIGINT, to prove the
        // `termination` feature of ctrlc is actually wired through.
        let out = run_and_signal(
            env!("CARGO_BIN_EXE_sigint_cancel_tokio_mt"),
            "sigint_cancel_ready_marker",
            libc::SIGTERM,
        );
        let log = log_of(&out);
        anyhow::ensure!(
            out.status.code() == Some(1),
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
        anyhow::ensure!(log.contains("1 cancelled"), "output:\n{log}");
        Ok(())
    }
}
