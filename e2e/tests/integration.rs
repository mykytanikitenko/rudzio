// Integration test helpers use `panic!` on unrecoverable I/O, integer
// literals typed by inference, and slice indexing for source-ordered
// lookups. These patterns are idiomatic for `#[test]` bodies even if
// pedantic / restriction lints would flag them elsewhere.
#![allow(
    clippy::panic,
    clippy::default_numeric_fallback,
    clippy::indexing_slicing,
    clippy::missing_asserts_for_indexing,
    clippy::min_ident_chars,
    reason = "idiomatic for #[test] bodies, not production code"
)]

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

#[test]
fn mut_test_context_is_borrowable() {
    let out = run(env!("CARGO_BIN_EXE_mutable_test_context"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("3 passed"), "output:\n{log}");
    assert!(log.contains("0 failed"), "output:\n{log}");
    assert!(log.contains("mutates_via_mut_borrow"), "output:\n{log}");
    assert!(log.contains("sync_mutates_via_mut_borrow"), "output:\n{log}");
    assert!(log.contains("fresh_ctx_per_test"), "output:\n{log}");
}

#[test]
fn passing_tokio_mt_succeeds() {
    let out = run(env!("CARGO_BIN_EXE_passing_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("first_passes"), "output:\n{log}");
    assert!(log.contains("second_passes"), "output:\n{log}");
    assert!(log.contains("... ok"), "output:\n{log}");
    assert!(log.contains("2 passed"), "output:\n{log}");
    assert!(log.contains("0 failed"), "output:\n{log}");
}

#[test]
fn failing_tokio_mt_exits_one() {
    let out = run(env!("CARGO_BIN_EXE_failing_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(1), "expected exit 1, output:\n{log}");
    assert!(log.contains("FAILED"), "output:\n{log}");
    assert!(log.contains("1 passed"), "output:\n{log}");
    assert!(log.contains("1 failed"), "output:\n{log}");
}

#[test]
fn ignored_tests_are_skipped() {
    let out = run(env!("CARGO_BIN_EXE_ignored_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("... ignored"), "output:\n{log}");
    assert!(log.contains("takes too long"), "output:\n{log}");
    assert!(log.contains("1 passed"), "output:\n{log}");
    assert!(log.contains("2 ignored"), "output:\n{log}");
    assert!(!log.contains("must not run"), "output:\n{log}");
}

#[test]
fn tokio_current_thread_runtime_works() {
    let out = run(env!("CARGO_BIN_EXE_passing_tokio_ct"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("yields_then_passes"), "output:\n{log}");
    assert!(log.contains("... ok"), "output:\n{log}");
}

#[test]
fn compio_runtime_works() {
    let out = run(env!("CARGO_BIN_EXE_passing_compio"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("passes_under_compio"), "output:\n{log}");
    assert!(log.contains("... ok"), "output:\n{log}");
}

#[test]
fn multi_runtime_runs_every_config() {
    let out = run(env!("CARGO_BIN_EXE_multi_runtime"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("Multithread"), "output:\n{log}");
    assert!(log.contains("CompioRuntime"), "output:\n{log}");
    assert!(log.contains("2 passed"), "output:\n{log}");
}

#[test]
fn panicking_test_does_not_halt_the_suite() {
    // Expected behavior: a panic in one test is isolated — subsequent tests
    // in the same runtime group still execute and the summary reports
    // 2 passed, 1 panicked, 3 total.
    let out = run(env!("CARGO_BIN_EXE_panics_tokio_mt"));
    let log = log_of(&out);
    assert!(log.contains("before_panic"), "output:\n{log}");
    assert!(
        log.contains("after_panic"),
        "after_panic never ran — panic killed the whole thread. output:\n{log}"
    );
    assert!(log.contains("2 passed"), "output:\n{log}");
    assert!(log.contains("1 panicked"), "output:\n{log}");
    assert!(log.contains("3 total"), "output:\n{log}");
}

#[test]
fn sync_tests_mix_pass_fail_panic() {
    // All three sync tests run; isolation keeps panics from killing the thread.
    let out = run(env!("CARGO_BIN_EXE_sync_mixed_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(1), "expected exit 1, output:\n{log}");
    assert!(log.contains("sync_passes"), "output:\n{log}");
    assert!(log.contains("sync_returns_err"), "output:\n{log}");
    assert!(log.contains("sync_panics"), "output:\n{log}");
    assert!(log.contains("1 passed"), "output:\n{log}");
    assert!(log.contains("1 failed"), "output:\n{log}");
    assert!(log.contains("1 panicked"), "output:\n{log}");
    assert!(log.contains("3 total"), "output:\n{log}");
}

#[test]
fn custom_global_and_test_impls_work() {
    let out = run(env!("CARGO_BIN_EXE_custom_context_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("runs_on_custom_context"), "output:\n{log}");
    assert!(log.contains("... ok"), "output:\n{log}");
    assert!(log.contains("1 passed"), "output:\n{log}");
}

#[test]
fn global_setup_failure_aborts_the_runtime_group() {
    let out = run(env!("CARGO_BIN_EXE_setup_failure_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(1), "expected exit 1, output:\n{log}");
    assert!(
        log.contains("FATAL: failed to create global context"),
        "output:\n{log}"
    );
    assert!(log.contains("setup_failed_by_design"), "output:\n{log}");
    assert!(log.contains("1 panicked"), "output:\n{log}");
    assert!(!log.contains("... ok"), "output:\n{log}");
}

#[test]
fn context_creation_failure_counts_as_failed() {
    let out = run(env!("CARGO_BIN_EXE_context_creation_failure_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(1), "expected exit 1, output:\n{log}");
    assert!(log.contains("first"), "output:\n{log}");
    assert!(log.contains("second"), "output:\n{log}");
    // The error's Display must be propagated through the failure output.
    assert!(
        log.contains("context_creation_failed_by_design"),
        "output:\n{log}"
    );
    assert!(log.contains("2 failed"), "output:\n{log}");
    assert!(log.contains("0 passed"), "output:\n{log}");
    assert!(log.contains("0 panicked"), "output:\n{log}");
}

#[test]
fn plain_test_attribute_is_recognized() {
    let out = run(env!("CARGO_BIN_EXE_plain_test_attr_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("runs_via_plain_test_attribute"), "output:\n{log}");
    assert!(log.contains("... ok"), "output:\n{log}");
    assert!(log.contains("1 passed"), "output:\n{log}");
}

#[test]
fn multiple_panics_are_isolated_and_ordered() {
    // RUST_TEST_THREADS=1 forces strict serial execution so the
    // source-order assertion below is meaningful.
    let out = run_serial(env!("CARGO_BIN_EXE_multi_panic_ordering_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(1), "expected exit 1, output:\n{log}");
    assert!(log.contains("3 passed"), "output:\n{log}");
    assert!(log.contains("2 panicked"), "output:\n{log}");
    assert!(log.contains("0 failed"), "output:\n{log}");
    assert!(log.contains("5 total"), "output:\n{log}");

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
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "tests ran out of order; positions {positions:?}, output:\n{log}"
    );
}

#[test]
fn tracker_drains_on_global_teardown() {
    let out = run(env!("CARGO_BIN_EXE_tracker_drain_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("... ok"), "output:\n{log}");
    // The marker only appears if global teardown waited for the tracked task.
    assert!(
        log.contains("tracker_drain_marker"),
        "tracker did not drain the spawned task; output:\n{log}"
    );
}

#[test]
fn per_test_teardown_cancels_the_cancel_token() {
    let out = run(env!("CARGO_BIN_EXE_cancel_token_propagation_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("... ok"), "output:\n{log}");
    assert!(
        log.contains("cancel_propagation_marker"),
        "teardown did not cancel the token; output:\n{log}"
    );
}

#[test]
fn spawn_tracked_test_passes() {
    let out = run(env!("CARGO_BIN_EXE_spawn_tracked"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("spawn_awaits_result"), "output:\n{log}");
    assert!(log.contains("cancel_token_is_child"), "output:\n{log}");
    assert!(log.contains("2 passed"), "output:\n{log}");
}

#[test]
fn sync_work_runs_on_the_runtimes_blocking_pool() {
    let out = run(env!("CARGO_BIN_EXE_spawn_blocking_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("runs_sync_fn_via_spawn_blocking"), "output:\n{log}");
    assert!(log.contains("spawn_blocking_uses_a_different_thread"), "output:\n{log}");
    assert!(log.contains("2 passed"), "output:\n{log}");
    assert!(log.contains("0 failed"), "output:\n{log}");
}

#[test]
fn parallel_runner_executes_tests_concurrently() {
    // The fixture has three tests that synchronise on a Barrier::new(3) with a 2s timeout.
    // If the runner dispatches all three concurrently, the barrier releases and every test passes.
    let out = run_parallel(env!("CARGO_BIN_EXE_parallel_tokio_mt"), 3);
    let log = log_of(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0 under parallel dispatch, output:\n{log}"
    );
    assert!(log.contains("3 passed"), "output:\n{log}");
    assert!(log.contains("0 failed"), "output:\n{log}");
}

#[test]
fn multiple_runtime_groups_run_in_separate_threads_concurrently() {
    // Fixture has two runtime groups (tokio multi-thread + compio), each with one test.
    // Both tests block on a shared Barrier(2) via spawn_blocking. If the groups' threads
    // run in parallel, both arrive at the barrier and it releases.
    let out = run(env!("CARGO_BIN_EXE_cross_runtime_parallel"));
    let log = log_of(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0 (watchdog exit 2 means groups serialised), output:\n{log}"
    );
    assert!(log.contains("Multithread"), "output:\n{log}");
    assert!(log.contains("CompioRuntime"), "output:\n{log}");
    assert!(log.contains("2 passed"), "output:\n{log}");
}

#[test]
fn teardown_runs_even_on_per_test_timeout() {
    // `--test-timeout=1` forces the test body to be cancelled by the
    // runner's watchdog. Both the per-test teardown and the global teardown
    // must still run — the integration asserts on their stdout markers.
    let out = run_serial_with_args(
        env!("CARGO_BIN_EXE_teardown_always_runs_tokio_mt"),
        &["--test-timeout=1"],
    );
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(1), "expected exit 1, output:\n{log}");
    assert!(log.contains("FAILED (timed out)"), "output:\n{log}");
    assert!(
        !log.contains("body_times_out_unreached_marker"),
        "test body must not complete, output:\n{log}"
    );
    assert!(
        log.contains("teardown_test_marker"),
        "per-test teardown must run after timeout, output:\n{log}"
    );
    assert!(
        log.contains("teardown_global_marker"),
        "global teardown must run after timeout, output:\n{log}"
    );
}

#[test]
fn per_test_timeout_cancels_only_the_offending_test() {
    // With `--test-timeout=1`, the per-test watchdog must fire only on the
    // hanging test; the other test in the same suite must still run.
    let out = run_serial_with_args(
        env!("CARGO_BIN_EXE_per_test_timeout_tokio_mt"),
        &["--test-timeout=1"],
    );
    let log = log_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on per-test timeout, output:\n{log}"
    );
    assert!(log.contains("FAILED (timed out)"), "output:\n{log}");
    assert!(
        !log.contains("hangs_until_timeout_unreached_marker"),
        "test body should not have completed, output:\n{log}"
    );
    assert!(
        log.contains("still_runs_after_previous_timeout_marker"),
        "subsequent test must still run, output:\n{log}"
    );
    assert!(log.contains("1 passed"), "output:\n{log}");
    assert!(log.contains("1 timed out"), "output:\n{log}");
}

#[test]
fn run_timeout_cancels_the_whole_run() {
    let out = run_serial_with_args(
        env!("CARGO_BIN_EXE_run_timeout_tokio_mt"),
        &["--run-timeout=1"],
    );
    let log = log_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 after run-timeout cancel, output:\n{log}"
    );
    assert!(
        log.contains("run timeout"),
        "expected a run-timeout diagnostic, output:\n{log}"
    );
    assert!(
        log.contains("waits_for_run_cancel_acknowledged_marker"),
        "first test should observe cancellation, output:\n{log}"
    );
    assert!(log.contains("never_starts_first [Multithread") && log.contains("cancelled"));
    assert!(log.contains("never_starts_second [Multithread") && log.contains("cancelled"));
    assert!(
        !log.contains("never_starts_first_unreached_marker"),
        "cancelled test body must not run, output:\n{log}"
    );
    assert!(log.contains("2 cancelled"), "output:\n{log}");
}

#[test]
fn gradual_cancellation_waits_for_tracked_tasks() {
    // The tracked background task prints its cleanup marker only when the
    // runner's global teardown drains the TaskTracker after root cancel.
    let out = run_serial_with_args(
        env!("CARGO_BIN_EXE_gradual_cancel_tokio_mt"),
        &["--run-timeout=1"],
    );
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(
        log.contains("gradual_cancel_cleanup_marker"),
        "cleanup marker missing — runner did not wait for graceful cancel, output:\n{log}"
    );
    assert!(log.contains("1 passed"), "output:\n{log}");
}

#[cfg(unix)]
#[test]
fn sigint_cancels_run_gracefully() {
    let out = run_and_signal(
        env!("CARGO_BIN_EXE_sigint_cancel_tokio_mt"),
        "sigint_cancel_ready_marker",
        libc::SIGINT,
    );
    let log = log_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 after SIGINT cancellation, output:\n{log}"
    );
    assert!(
        log.contains("received SIGINT"),
        "expected the runner's signal diagnostic, output:\n{log}"
    );
    assert!(
        log.contains("sigint_cancel_observed_marker"),
        "test body must see cancellation via its context token, output:\n{log}"
    );
    assert!(
        !log.contains("never_runs_after_sigint_unreached_marker"),
        "queued test must not start after SIGINT, output:\n{log}"
    );
    assert!(log.contains("1 cancelled"), "output:\n{log}");
}

#[cfg(unix)]
#[test]
fn sigterm_cancels_run_gracefully() {
    // Same fixture, delivered SIGTERM instead of SIGINT, to prove the
    // `termination` feature of ctrlc is actually wired through.
    let out = run_and_signal(
        env!("CARGO_BIN_EXE_sigint_cancel_tokio_mt"),
        "sigint_cancel_ready_marker",
        libc::SIGTERM,
    );
    let log = log_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 after SIGTERM cancellation, output:\n{log}"
    );
    assert!(
        log.contains("received SIGTERM"),
        "expected the runner's signal diagnostic, output:\n{log}"
    );
    assert!(log.contains("sigint_cancel_observed_marker"), "output:\n{log}");
    assert!(
        !log.contains("never_runs_after_sigint_unreached_marker"),
        "output:\n{log}"
    );
    assert!(log.contains("1 cancelled"), "output:\n{log}");
}

#[test]
fn multi_file_suites_share_one_main_runner() {
    // Tokens registered by separate source files must all show up in a
    // single run under a single `rudzio::run()` call.
    let out = run(env!("CARGO_BIN_EXE_multi_file_suite"));
    let log = log_of(&out);
    assert_eq!(out.status.code(), Some(0), "expected exit 0, output:\n{log}");
    assert!(log.contains("module_a_first"), "output:\n{log}");
    assert!(log.contains("module_a_second"), "output:\n{log}");
    assert!(log.contains("module_b_first"), "output:\n{log}");
    assert!(log.contains("module_b_second"), "output:\n{log}");
    assert!(log.contains("4 passed"), "output:\n{log}");
    assert!(log.contains("0 failed"), "output:\n{log}");
    assert!(log.contains("4 total"), "output:\n{log}");
}

#[test]
fn serial_runner_does_not_execute_tests_concurrently() {
    // With RUST_TEST_THREADS=1 the barrier fixture must fail:
    // the first test hits the barrier and times out before the others arrive.
    let out = run_serial(env!("CARGO_BIN_EXE_parallel_tokio_mt"));
    let log = log_of(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 under serial dispatch, output:\n{log}"
    );
    assert!(
        log.contains("barrier timed out"),
        "expected a barrier-timeout failure, output:\n{log}"
    );
}
