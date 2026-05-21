//! Bug #13 regression fixture.
//!
//! Initialises the rudzio capture pipeline, spawns a long-lived child
//! process that inherits FDs 1 and 2 (the pipe write ends after
//! `output::init`), then drops the [`CaptureGuard`] and exits.
//!
//! # The bug it pins
//!
//! Pre-fix `CaptureGuard::drop` joins the reader threads. The
//! reader threads exit only when `read()` returns 0 (EOF), which the
//! kernel reports only when *every* write-end FD is closed. The
//! child process inherited the pipe write ends and is still running,
//! so the reader's `read()` blocks forever — the parent process
//! hangs at drop and never exits. This is the root cause of the
//! multi-binary monolith hang the user reported: each binary's
//! suite setup spawns a child, and the cumulative capture-guard
//! teardown blocks on any one of them.
//!
//! # Expected behaviour
//!
//! Post-fix the fixture process exits cleanly within a small bound
//! (well under 5s). The integration test reads the log file the
//! fixture writes to, asserts the drop returned, and SIGKILLs the
//! orphaned child via its harvested pid.
//!
//! Diagnostics are written to the file path given in the
//! `RUDZIO_CAPTURE_TEST_LOG` env var rather than to stdout/stderr.
//! That avoids a second layer of pipe inheritance back to the test
//! process when the test wires `Stdio::piped()` for capture — the
//! orphaned `sleep` child would otherwise inherit the test's outer
//! pipe FDs (via the saved-FD `dup`s in `output::init`, which lack
//! `FD_CLOEXEC`) and block `read_to_end` in the test wrapper.

use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::{Error as IoError, Result as IoResult, Write as _};
use std::process::{Child, Command};
use std::time::Instant;

use rudzio::Config;
use rudzio::cargo_meta;
use rudzio::output;

fn main() -> IoResult<()> {
    let log_path = env::var("RUDZIO_CAPTURE_TEST_LOG")
        .map_err(|_var_err| IoError::other("RUDZIO_CAPTURE_TEST_LOG must be set by caller"))?;
    let mut log = File::create(&log_path)?;

    // Force Live output mode so the Unix capture path actually runs.
    // Without an explicit `--output=live`, a TTY-less subprocess
    // invocation resolves to Plain mode (no-op guard) and the
    // regression check would trivially pass.
    let argv: Vec<String> = vec!["fixture".to_owned(), "--output=live".to_owned()];
    let env_snapshot: BTreeMap<String, String> = env::vars().collect();
    let config = Config::from_argv_and_env(&argv, env_snapshot, cargo_meta!());

    // Initialise capture. FDs 1 and 2 are now pipe write ends without
    // `FD_CLOEXEC` (`dup2` clears it; the saved originals returned
    // by `dup()` also lack it). Anything we `spawn()` with the
    // default `Stdio::inherit()` will inherit those FDs.
    let guard = output::init(&config)?;

    // Spawn the child process that holds the inherited pipe FDs.
    // `sleep 600` is plenty: the integration test will SIGKILL it.
    let child: Child = Command::new("sleep").arg("600").spawn()?;
    writeln!(log, "rudzio_capture_test_child_pid={}", child.id())?;
    log.sync_data()?;

    // Drop the guard, timing how long it takes. Pre-fix: blocks
    // forever on reader.join() because the child holds the pipe
    // write end. Post-fix: returns quickly because the readers are
    // detached.
    let start = Instant::now();
    drop(guard);
    let elapsed = start.elapsed();

    // FDs 1 and 2 have been restored by the drop. Any prints from
    // this point go to the original stdout/stderr (which the test
    // wired to `Stdio::null()`). The log file is the authoritative
    // diagnostic channel.
    writeln!(
        log,
        "rudzio_capture_drop_returned_in_ms={}",
        elapsed.as_millis()
    )?;
    log.sync_data()?;

    Ok(())
}
