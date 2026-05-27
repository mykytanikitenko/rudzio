//! Reproducer for Bug #14: `cargo rudzio test` against a crate with
//! TWO `[[test]]` binaries — each declaring its own `Suite` that
//! spawns a long-lived child process and drains the child's piped
//! stdout/stderr via background tasks — exits with code **137
//! (SIGKILL — 128 + 9)** mid-stream, with no `test result:` line, no
//! teardown, no panic, no OOM message. Single-binary invocations
//! (`--test alpha` or `--test beta`) work fine; only the combined
//! `cargo rudzio test` form trips the kill.
//!
//! Linux-only: `process_group(0)` + `killpg` are Unix-only and we need
//! the process-group reach to reap any orphaned children if the test
//! safety bound expires; the bug itself is Unix-shaped (file-v3 plus
//! Docker on Linux). macOS is in scope but un-tested here; Windows is
//! out of scope.
#![cfg(unix)]
//!
//! # Difference from Bug #13
//!
//! `9aaf7ab` fixed a HANG caused by capture-pipe FD inheritance --
//! the runner could not drop its capture pipes because a spawned
//! child still held the read end open. This is a different mode:
//! the runner is actively `SIGKILL`-ed mid-stream. `SIGKILL` means
//! either an external killer (systemd-oomd, ulimit watchdog) or a
//! deliberate `_exit` somewhere. Rudzio's own force-exit path uses
//! `libc::_exit(2)`, so it can't be the culprit (137, not 2).
//!
//! # Shape of the synthetic repro
//!
//! Consumer crate with:
//!  * `[[bin]] sigkill_repro_child` -- the long-lived child, prints
//!    `alive` lines until `SIGTERM`/`SIGKILL`.
//!  * `[[test]] alpha` -- its own `AlphaSuite` spawns the child and
//!    drains both pipes; `TESTS_PER_BINARY` trivial `#[rudzio::test]`
//!    fns.
//!  * `[[test]] beta` -- same shape with `BetaSuite`. Different type
//!    name is mandatory: cargo-rudzio's aggregator groups by `(runtime,
//!    suite, test)` tuple. A SHARED `Suite` collapses both binaries
//!    into one runtime group with ONE `setup` call; distinct types
//!    keep them as TWO groups, so BOTH children are alive
//!    simultaneously while the merged test stream runs. The
//!    real-world bug (`file-v3` + `fathom_containers`) has distinct
//!    `integration::Suite` and `e2e::Suite`, mirrored here.
//!
//! # Assertion
//!
//! Under the synthetic configuration, `cargo rudzio test` against
//! the workspace must:
//!  * exit with a code other than 137 (pre-fix: 137; post-fix:
//!    0 if every test passes, 1 if any fails -- never 137).
//!  * emit a `test result:` line on stdout/stderr (proves the runner
//!    reached the summary phase rather than being killed before it).
//!
//! Currently this test EXPECTS the bug to reproduce -- i.e. it will
//! fail today (exit 137, no summary) and pass post-fix. The synthetic
//! fixture's resource pressure is intentionally lower than the
//! real-world `file-v3` setup (no Docker, no `fathom_containers`); if
//! that lower pressure is insufficient to trigger the kill on a given
//! host, the test passes trivially and acts as a forward-looking
//! regression guard for hosts where it does trigger.
//!
//! # Why not just check exit code?
//!
//! A non-137 exit isn't sufficient: a synthetic crate that fails to
//! compile would exit 1 too. We require BOTH non-137 exit AND a
//! visible `test result:` line, so a failing compile fails the test
//! with a clear diagnostic.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rudzio::runtime::futures::ThreadPool;

use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{async_std, compio, embassy, smol};
use tempfile::TempDir;

/// Number of trivial `#[rudzio::test]` fns generated per test
/// binary. The real-world repro (file-v3 + e2e) trips SIGKILL after
/// ~80 tests from one binary; 100/binary stays comfortably above
/// that threshold while keeping the synthetic build/run time bounded.
const TESTS_PER_BINARY: usize = 100;

/// How the per-binary `Suite` type is named in defensive variants:
/// distinct per binary (each binary becomes its own runtime group in
/// the aggregator) vs shared across binaries (unified into ONE
/// runtime group, ONE `Suite::setup` call).
#[derive(Clone, Copy)]
enum SuiteKind {
    /// Each binary declares its own `<Cap>Suite` / `<Cap>Test` pair.
    /// cargo-rudzio's aggregator sees N distinct runtime groups.
    Distinct,
    /// All binaries import the same `SharedSuite` / `SharedTest` from
    /// the consumer crate's lib. Aggregator unifies them into ONE
    /// runtime group with ONE `Suite::setup` call.
    Shared,
}

/// What the per-binary `Suite::setup` body does.
#[derive(Clone, Copy)]
enum SetupKind {
    /// Setup constructs the Suite struct but spawns no child process.
    /// Isolates "does child-process spawn matter for the bug?".
    NoChild,
    /// Setup spawns the long-lived child binary; `pipe` and `drain`
    /// vary the FD-handling surface for further single-axis isolation.
    SpawnChild {
        /// Whether to consume the piped pipes via background reader
        /// tasks. `false` lets pipes fill (Linux pipe buffer ~64KB)
        /// until the child blocks on a write.
        drain: bool,
        /// Whether the child's stdout/stderr are inherited as pipes
        /// or redirected to `/dev/null`.
        pipe: PipeMode,
    },
}

/// Whether the child's stdout/stderr are inherited as pipes or
/// redirected to `/dev/null`. `Null` isolates "does piping matter?".
#[derive(Clone, Copy)]
enum PipeMode {
    /// `Stdio::null()` for both stdout and stderr.
    Null,
    /// `Stdio::piped()` for both stdout and stderr.
    Piped,
}

/// Which host async runtime the per-binary `#[rudzio::suite]` declares.
/// `CrossRuntime` variants use `Multithread` for one binary and
/// `CurrentThread` for the other to force two genuinely-distinct
/// runtime groups (instead of relying on distinct Suite types alone).
#[derive(Clone, Copy)]
enum HostRuntime {
    /// Tokio current-thread runtime.
    CurrentThread,
    /// Tokio multi-thread runtime.
    Multithread,
}

/// Whether the child binary writes output to its stdio streams or runs
/// silently. `Quiet` isolates "does child output volume matter?".
#[derive(Clone, Copy)]
enum ChildWriteMode {
    /// Child sleeps in a loop, writes nothing.
    Quiet,
    /// Child writes `alive N` to stdout and `diag N` to stderr every
    /// 50ms.
    Verbose,
}

/// Per-binary fixture configuration.
#[derive(Clone)]
struct BinaryOpts {
    /// Host async runtime the suite declares.
    host_runtime: HostRuntime,
    /// Lowercase binary name; also the directory name under `tests/`.
    name: &'static str,
    /// What `Suite::setup` does.
    setup: SetupKind,
    /// Distinct or shared `<Suite>` / `<Test>` types.
    suite_kind: SuiteKind,
    /// Number of trivial `#[rudzio::test]` fns generated.
    tests: usize,
}

impl BinaryOpts {
    /// Construct the "primary" shape: distinct Suite, spawns child
    /// with piped + drained pipes, full test count, Multithread.
    /// Mirrors the user-reported `file-v3` setup.
    const fn primary(name: &'static str) -> Self {
        Self {
            host_runtime: HostRuntime::Multithread,
            name,
            setup: SetupKind::SpawnChild {
                drain: true,
                pipe: PipeMode::Piped,
            },
            suite_kind: SuiteKind::Distinct,
            tests: TESTS_PER_BINARY,
        }
    }
}

/// Full-workspace fixture configuration.
#[derive(Clone)]
struct FixtureOpts {
    /// One entry per `[[test]]` binary in the synthetic consumer.
    binaries: Vec<BinaryOpts>,
    /// Whether the child writes any output. Setting this `Quiet`
    /// AND `pipe == Piped` AND `drain == true` keeps the FD shape
    /// intact while removing the I/O volume.
    child_writes: ChildWriteMode,
}

impl FixtureOpts {
    /// The standard Bug #14 primary repro: 2 binaries, distinct
    /// Suites, full child spawn + pipe + drain, 100 tests each,
    /// verbose child output.
    fn primary() -> Self {
        Self {
            binaries: vec![BinaryOpts::primary("alpha"), BinaryOpts::primary("beta")],
            child_writes: ChildWriteMode::Verbose,
        }
    }
}

/// Workspace root of the rudzio repo this test lives in.
///
/// Synthetic fixtures reference `rudzio` by `path = <this>` so
/// `cargo metadata` resolves the dep without network. See
/// `build_dir_deadlock_spec.rs` for the `manifest_dir!()` rationale.
fn rudzio_source_root() -> PathBuf {
    rudzio::manifest_dir!().join("..")
}

/// Emit `count` trivial `#[rudzio::test]` fns named
/// `<prefix>_test_<NNN>`; each returns `Ok(())`. Names are zero-padded
/// to three digits so the lexicographic order matches numeric order
/// (cargo-rudzio sorts tests alphabetically by default), which keeps
/// the "after ~80 tests" observation in the bug report meaningful.
/// `test_type` is the path to the per-test context type the synthetic
/// test fns accept by reference — `<Cap>Test` for distinct suites or
/// `SharedTest` for the shared-suite variant.
fn generate_test_fns(prefix: &str, count: usize, test_type: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for index in 1..=count {
        let _written = writeln!(
            out,
            "    #[rudzio::test]\n    fn {prefix}_test_{index:03}(_t: &super::{test_type}) -> anyhow::Result<()> {{ Ok(()) }}"
        );
    }
    out
}

/// Path expression for the runtime type in the `#[rudzio::suite]`
/// list, given a host runtime choice.
const fn host_runtime_path(rt: HostRuntime) -> &'static str {
    match rt {
        HostRuntime::Multithread => "Multithread",
        HostRuntime::CurrentThread => "CurrentThread",
    }
}

/// `use` line that brings the chosen host runtime into scope at the
/// top of the generated mod.rs.
const fn host_runtime_import(rt: HostRuntime) -> &'static str {
    match rt {
        HostRuntime::Multithread => "use rudzio::runtime::tokio::Multithread;",
        HostRuntime::CurrentThread => "use rudzio::runtime::tokio::CurrentThread;",
    }
}

/// `"alpha"` -> `"Alpha"`. Suite/Test type names match the binary
/// name with the first letter uppercased.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Materialise a `tests/<bin>/mod.rs` for ONE test binary based on
/// `opts`. Branches on `opts.suite_kind` first (distinct vs shared
/// suite type), then within the distinct branch on `opts.setup` for
/// the setup body and the imports it needs.
fn generate_test_binary_mod_rs(opts: &BinaryOpts) -> String {
    match opts.suite_kind {
        SuiteKind::Distinct => generate_test_binary_mod_rs_distinct(opts),
        SuiteKind::Shared => generate_test_binary_mod_rs_shared(opts),
    }
}

/// Distinct-suite variant: defines `<Cap>Suite` + `<Cap>Test` inline
/// in the binary's mod.rs. The `Suite::setup` body is parameterized
/// on `opts.setup` to support the no-child / unpiped / no-drain
/// isolation variants.
#[expect(
    clippy::too_many_lines,
    reason = "this fn emits a ~200-line Rust source-code template via format!; splitting it into smaller helpers would fragment the template into less-readable chunks and break the 1:1 correspondence between the function body and the generated mod.rs"
)]
fn generate_test_binary_mod_rs_distinct(opts: &BinaryOpts) -> String {
    let cap = capitalize(opts.name);
    let test_fns = generate_test_fns(opts.name, opts.tests, &format!("{cap}Test"));
    let rt_path = host_runtime_path(opts.host_runtime);
    let rt_import = host_runtime_import(opts.host_runtime);

    let (process_imports, child_field, setup_body, teardown_body) = match opts.setup {
        SetupKind::NoChild => (
            // No `tokio::process::Command` / `Stdio` / `AsyncReadExt`
            // imports — they'd be unused with `NoChild` and trigger
            // the workspace `unused_imports` deny.
            String::new(),
            String::new(),
            "        Ok(Self {
            cancel: cancel.child_token(),
            rt,
            tracker: TaskTracker::new(),
        })"
            .to_owned(),
            "        // No child to kill in this variant.
        let _closed = self.tracker.close();
        self.tracker.wait().await;
        self.cancel.cancel();
        Ok(())"
                .to_owned(),
        ),
        SetupKind::SpawnChild { pipe, drain } => {
            let imports = "use std::process::Stdio;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command as TokioCommand;
"
            .to_owned();
            let child_field = "    /// Long-lived child process spawned in `setup`. Wrapped in
    /// `Option` so `teardown` can `take()` the handle and `kill().await`
    /// it consuming-style.
    child: ::std::sync::Mutex<Option<tokio::process::Child>>,
"
            .to_owned();
            let stdio_setup = match pipe {
                PipeMode::Piped => ".stdout(Stdio::piped())\n            .stderr(Stdio::piped())",
                PipeMode::Null => ".stdout(Stdio::null())\n            .stderr(Stdio::null())",
            };
            let drain_block = match (pipe, drain) {
                (PipeMode::Piped, true) => "        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!(\"child stdout missing\"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!(\"child stderr missing\"))?;
        let _drain_stdout = tracker.spawn(async move {
            let mut reader = stdout;
            let mut buf = [0u8; 1024];
            while reader.read(&mut buf).await.unwrap_or(0) > 0 {}
        });
        let _drain_stderr = tracker.spawn(async move {
            let mut reader = stderr;
            let mut buf = [0u8; 1024];
            while reader.read(&mut buf).await.unwrap_or(0) > 0 {}
        });
"
                .to_owned(),
                (PipeMode::Piped, false) => {
                    "        // Piped but NOT drained -- child will eventually block on write.
        // The pipe handles stay attached to the suite via the live
        // `Child` handle without being read. Isolates whether the
        // drain task itself is part of the bug trigger.
        let _stdout = child.stdout.take();
        let _stderr = child.stderr.take();
"
                    .to_owned()
                }
                (PipeMode::Null, _) => "        // Stdio::null() -- no pipes to drain.
"
                .to_owned(),
            };
            let setup = format!(
                "        // Locate `sigkill_repro_child` at runtime. `env!()` won't work
        // because `CARGO_BIN_EXE_*` is only set in the env of cargo-driven
        // test/bench/example target compiles, NOT in the aggregator binary
        // (`rudzio-auto-runner`) that ends up compiling this proxy mod.
        // Layout: aggregator runs from `target/<profile>/rudzio-auto-runner`;
        // member bins land at `target/<profile>/<bin_name>`. When invoked
        // as a regular cargo test binary, current_exe() is in `deps/` --
        // strip that one segment.
        let exe = std::env::current_exe()
            .map_err(|err| anyhow::anyhow!(\"current_exe failed: {{err}}\"))?;
        let parent = exe
            .parent()
            .ok_or_else(|| anyhow::anyhow!(\"current_exe has no parent\"))?;
        let target_dir = if parent.file_name().and_then(|name| name.to_str()) == Some(\"deps\") {{
            parent
                .parent()
                .ok_or_else(|| anyhow::anyhow!(\"`deps` has no parent\"))?
        }} else {{
            parent
        }};
        let child_bin = target_dir.join(\"sigkill_repro_child\");
        let mut child = TokioCommand::new(&child_bin)
            {stdio_setup}
            .spawn()
            .map_err(|err| anyhow::anyhow!(\"spawn child ({{}}): {{err}}\", child_bin.display()))?;
        let tracker = TaskTracker::new();
{drain_block}        Ok(Self {{
            cancel: cancel.child_token(),
            child: ::std::sync::Mutex::new(Some(child)),
            rt,
            tracker,
        }})"
            );
            let teardown = "        let extracted: Option<tokio::process::Child> = self
            .child
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(mut child) = extracted {
            drop(child.kill().await);
            drop(child.wait().await);
        }
        let _closed = self.tracker.close();
        self.tracker.wait().await;
        self.cancel.cancel();
        Ok(())"
                .to_owned();
            (imports, child_field, setup, teardown)
        }
    };

    format!(
        r#"//! Synthetic test binary `{bin}`. Variant generated by Bug #14
//! defensive test infrastructure.

use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
{rt_import}
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;
{process_imports}

pub struct {cap}Suite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{{
    cancel: CancellationToken,
{child_field}    rt: &'suite_context R,
    tracker: TaskTracker,
}}

pub struct {cap}Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{{
    cancel: CancellationToken,
    config: &'test_context Config,
    rt: &'test_context R,
    tracker: TaskTracker,
}}

impl<'suite_context, R> ::std::fmt::Debug for {cap}Suite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{{
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {{
        f.debug_struct("{cap}Suite").finish_non_exhaustive()
    }}
}}

impl<'test_context, R> ::std::fmt::Debug for {cap}Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{{
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {{
        f.debug_struct("{cap}Test").finish_non_exhaustive()
    }}
}}

impl<'suite_context, R> context::Suite<'suite_context, R> for {cap}Suite<'suite_context, R>
where
    R: for<'rt> Runtime<'rt> + Sync,
{{
    type ContextError = ::std::convert::Infallible;
    type SetupError = anyhow::Error;
    type TeardownError = ::std::convert::Infallible;
    type Test<'test_context>
        = {cap}Test<'test_context, R>
    where
        Self: 'test_context;

    fn cancel_token(&self) -> &CancellationToken {{
        &self.cancel
    }}

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {{
        Ok({cap}Test {{
            cancel,
            config,
            rt: self.rt,
            tracker: self.tracker.clone(),
        }})
    }}

    fn rt(&self) -> &'suite_context R {{
        self.rt
    }}

    async fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        _config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {{
{setup_body}
    }}

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {{
{teardown_body}
    }}

    fn tracker(&self) -> &TaskTracker {{
        &self.tracker
    }}
}}

impl<'test_context, R> context::Test<'test_context, R> for {cap}Test<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{{
    type TeardownError = ::std::convert::Infallible;

    fn cancel_token(&self) -> &CancellationToken {{
        &self.cancel
    }}

    fn config(&self) -> &Config {{
        self.config
    }}

    fn rt(&self) -> &'test_context R {{
        self.rt
    }}

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {{
        Ok(())
    }}

    fn tracker(&self) -> &TaskTracker {{
        &self.tracker
    }}
}}

#[rudzio::suite([
    (runtime = {rt_path}::new, suite = {cap}Suite, test = {cap}Test),
])]
mod tests {{
{test_fns}}}
"#,
        bin = opts.name,
    )
}

/// Shared-suite variant: imports `consumer::shared::SharedSuite` /
/// `SharedTest`. Both binaries in this variant use the same Suite type
/// → cargo-rudzio's aggregator unifies them into ONE runtime group →
/// only ONE `Suite::setup` runs → only ONE child process is alive
/// across both binaries' tests. Controls for "distinct Suite types
/// are required to trigger the bug".
fn generate_test_binary_mod_rs_shared(opts: &BinaryOpts) -> String {
    let test_fns = generate_test_fns(opts.name, opts.tests, "consumer::shared::SharedTest");
    let rt_path = host_runtime_path(opts.host_runtime);
    let rt_import = host_runtime_import(opts.host_runtime);
    format!(
        "//! Synthetic test binary `{bin}` -- shared-suite variant. Uses
//! `consumer::shared::SharedSuite`. The aggregator unifies both
//! binaries into ONE runtime group.

{rt_import}

use consumer::shared::{{SharedSuite, SharedTest}};

#[rudzio::suite([
    (runtime = {rt_path}::new, suite = SharedSuite, test = SharedTest),
])]
mod tests {{
{test_fns}}}
",
        bin = opts.name,
    )
}

/// Generate the `rudzio` features list for the workspace Cargo.toml,
/// driven by which host runtimes the binaries declare.
/// `CurrentThread` binaries need the `runtime-tokio-current-thread`
/// feature.
fn workspace_rudzio_features(opts: &FixtureOpts) -> String {
    let mut feats: Vec<&'static str> = vec!["common", "runtime-tokio-multi-thread"];
    if opts
        .binaries
        .iter()
        .any(|bin| matches!(bin.host_runtime, HostRuntime::CurrentThread))
    {
        feats.push("runtime-tokio-current-thread");
    }
    feats
        .iter()
        .map(|feature| format!("\"{feature}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Generate `consumer/src/lib.rs`. Includes a `pub mod shared`
/// defining `SharedSuite` + `SharedTest` when at least one binary
/// uses `SuiteKind::Shared`. Otherwise, a no-op stub.
#[expect(
    clippy::too_many_lines,
    reason = "this fn emits a ~150-line Rust source-code template defining `SharedSuite` + `SharedTest`; splitting would fragment the template"
)]
fn generate_consumer_lib_rs(opts: &FixtureOpts) -> String {
    let needs_shared = opts
        .binaries
        .iter()
        .any(|bin| matches!(bin.suite_kind, SuiteKind::Shared));
    if !needs_shared {
        return "//! Synthetic Bug #14 consumer. The bin and tests carry the\n\
                //! payload; this lib is a no-op required to ground the crate.\n"
            .to_owned();
    }
    // Same shape as the distinct `<Cap>Suite` but with a fixed name.
    // Used by the SuiteKind::Shared variant -- both binaries import
    // SharedSuite/SharedTest from here.
    //
    // The child bin path is resolved at RUNTIME via
    // `std::env::current_exe()` (NOT compile-time `env!()`) because
    // `CARGO_BIN_EXE_*` is only set in the env of test/bench/example
    // targets, not the consumer library. `current_exe()` returns the
    // aggregator binary's path; the child bin lives in the same
    // `target/<profile>/` directory.
    r#"//! Synthetic Bug #14 consumer with a SHARED Suite type. Both
//! test binaries that use `SuiteKind::Shared` import the same
//! `SharedSuite` / `SharedTest` from this module, so cargo-rudzio's
//! aggregator unifies them into ONE runtime group.

pub mod shared {
    use std::path::PathBuf;
    use std::process::Stdio;

    use rudzio::Config;
    use rudzio::context;
    use rudzio::runtime::Runtime;
    use rudzio::tokio_util::sync::CancellationToken;
    use rudzio::tokio_util::task::TaskTracker;
    use tokio::io::AsyncReadExt as _;
    use tokio::process::Command as TokioCommand;

    /// Locate `sigkill_repro_child` relative to the running aggregator
    /// binary. `env!()` is unavailable here because lib.rs is compiled
    /// in the consumer crate's env (not a test target's env), so we
    /// use `current_exe()` + cargo's known target-dir layout:
    ///   * aggregator runs from `target/<profile>/rudzio-auto-runner`
    ///   * member bins land at `target/<profile>/<bin_name>`
    /// When invoked as a regular cargo test binary, the path is
    /// `target/<profile>/deps/<test>-HASH` -- we strip the trailing
    /// `deps/` segment in that case.
    fn locate_child_bin() -> anyhow::Result<PathBuf> {
        let exe = std::env::current_exe()
            .map_err(|err| anyhow::anyhow!("current_exe failed: {err}"))?;
        let parent = exe
            .parent()
            .ok_or_else(|| anyhow::anyhow!("current_exe has no parent: {}", exe.display()))?;
        let target_dir = if parent.file_name().and_then(|name| name.to_str()) == Some("deps") {
            parent
                .parent()
                .ok_or_else(|| anyhow::anyhow!("`deps` has no parent"))?
        } else {
            parent
        };
        Ok(target_dir.join("sigkill_repro_child"))
    }

    pub struct SharedSuite<'suite_context, R>
    where
        R: Runtime<'suite_context> + Sync,
    {
        cancel: CancellationToken,
        child: ::std::sync::Mutex<Option<tokio::process::Child>>,
        rt: &'suite_context R,
        tracker: TaskTracker,
    }

    pub struct SharedTest<'test_context, R>
    where
        R: Runtime<'test_context> + Sync,
    {
        cancel: CancellationToken,
        config: &'test_context Config,
        rt: &'test_context R,
        tracker: TaskTracker,
    }

    impl<'suite_context, R> ::std::fmt::Debug for SharedSuite<'suite_context, R>
    where
        R: Runtime<'suite_context> + Sync,
    {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
            f.debug_struct("SharedSuite").finish_non_exhaustive()
        }
    }

    impl<'test_context, R> ::std::fmt::Debug for SharedTest<'test_context, R>
    where
        R: Runtime<'test_context> + Sync,
    {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
            f.debug_struct("SharedTest").finish_non_exhaustive()
        }
    }

    impl<'suite_context, R> context::Suite<'suite_context, R> for SharedSuite<'suite_context, R>
    where
        R: for<'rt> Runtime<'rt> + Sync,
    {
        type ContextError = ::std::convert::Infallible;
        type SetupError = anyhow::Error;
        type TeardownError = ::std::convert::Infallible;
        type Test<'test_context>
            = SharedTest<'test_context, R>
        where
            Self: 'test_context;

        fn cancel_token(&self) -> &CancellationToken {
            &self.cancel
        }

        async fn context<'test_context>(
            &'test_context self,
            cancel: CancellationToken,
            config: &'test_context Config,
        ) -> Result<Self::Test<'test_context>, Self::ContextError> {
            Ok(SharedTest {
                cancel,
                config,
                rt: self.rt,
                tracker: self.tracker.clone(),
            })
        }

        fn rt(&self) -> &'suite_context R {
            self.rt
        }

        async fn setup(
            rt: &'suite_context R,
            cancel: CancellationToken,
            _config: &'suite_context Config,
        ) -> Result<Self, Self::SetupError> {
            let child_bin = locate_child_bin()?;
            let mut child = TokioCommand::new(&child_bin)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|err| anyhow::anyhow!("spawn child ({}): {err}", child_bin.display()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("child stdout missing"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| anyhow::anyhow!("child stderr missing"))?;
            let tracker = TaskTracker::new();
            let _drain_stdout = tracker.spawn(async move {
                let mut reader = stdout;
                let mut buf = [0u8; 1024];
                while reader.read(&mut buf).await.unwrap_or(0) > 0 {}
            });
            let _drain_stderr = tracker.spawn(async move {
                let mut reader = stderr;
                let mut buf = [0u8; 1024];
                while reader.read(&mut buf).await.unwrap_or(0) > 0 {}
            });
            Ok(Self {
                cancel: cancel.child_token(),
                child: ::std::sync::Mutex::new(Some(child)),
                rt,
                tracker,
            })
        }

        async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
            let extracted: Option<tokio::process::Child> = self
                .child
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(mut child) = extracted {
                drop(child.kill().await);
                drop(child.wait().await);
            }
            let _closed = self.tracker.close();
            self.tracker.wait().await;
            self.cancel.cancel();
            Ok(())
        }

        fn tracker(&self) -> &TaskTracker {
            &self.tracker
        }
    }

    impl<'test_context, R> context::Test<'test_context, R> for SharedTest<'test_context, R>
    where
        R: Runtime<'test_context> + Sync,
    {
        type TeardownError = ::std::convert::Infallible;

        fn cancel_token(&self) -> &CancellationToken {
            &self.cancel
        }

        fn config(&self) -> &Config {
            self.config
        }

        fn rt(&self) -> &'test_context R {
            self.rt
        }

        async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
            Ok(())
        }

        fn tracker(&self) -> &TaskTracker {
            &self.tracker
        }
    }
}
"#
    .to_owned()
}

/// Generate `consumer/Cargo.toml`. Builds the `[[test]]` table from
/// `opts.binaries`; includes `sigkill_repro_child` bin iff at least
/// one binary's setup spawns it.
fn generate_consumer_cargo_toml(opts: &FixtureOpts) -> String {
    let needs_child_bin = opts
        .binaries
        .iter()
        .any(|bin| matches!(bin.setup, SetupKind::SpawnChild { .. }))
        || opts
            .binaries
            .iter()
            .any(|bin| matches!(bin.suite_kind, SuiteKind::Shared));
    let mut out = String::new();
    out.push_str(
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         autotests = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n",
    );
    if needs_child_bin {
        out.push_str(
            "[[bin]]\n\
             name = \"sigkill_repro_child\"\n\
             path = \"src/bin/sigkill_repro_child.rs\"\n\
             test = false\n\
             \n",
        );
    }
    {
        use std::fmt::Write as _;
        for bin in &opts.binaries {
            let _written = write!(
                out,
                "[[test]]\n\
                 name = \"{name}\"\n\
                 path = \"tests/{name}/mod.rs\"\n\
                 harness = false\n\
                 \n",
                name = bin.name,
            );
        }
    }
    out.push_str(
        "[dependencies]\n\
         anyhow = { workspace = true }\n\
         rudzio = { workspace = true }\n\
         tokio = { workspace = true }\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         tokio = { workspace = true }\n",
    );
    out
}

/// Generate `src/bin/sigkill_repro_child.rs`. Verbose vs Quiet write
/// mode determined by `opts.child_writes`.
fn generate_child_bin_rs(opts: &FixtureOpts) -> String {
    match opts.child_writes {
        ChildWriteMode::Verbose => {
            "use std::io::Write as _;\n\
             use std::thread;\n\
             use std::time::Duration;\n\
             \n\
             fn main() {\n\
             \x20\x20\x20\x20let mut counter: u64 = 0;\n\
             \x20\x20\x20\x20loop {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20counter = counter.wrapping_add(1);\n\
             \x20\x20\x20\x20\x20\x20\x20\x20let stdout = std::io::stdout();\n\
             \x20\x20\x20\x20\x20\x20\x20\x20if writeln!(stdout.lock(), \"alive {counter}\").is_err() {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20return;\n\
             \x20\x20\x20\x20\x20\x20\x20\x20}\n\
             \x20\x20\x20\x20\x20\x20\x20\x20if counter % 10 == 0 {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20let stderr = std::io::stderr();\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20if writeln!(stderr.lock(), \"diag {counter}\").is_err() {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20return;\n\
             \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20}\n\
             \x20\x20\x20\x20\x20\x20\x20\x20}\n\
             \x20\x20\x20\x20\x20\x20\x20\x20thread::sleep(Duration::from_millis(50));\n\
             \x20\x20\x20\x20}\n\
             }\n"
                .to_owned()
        }
        ChildWriteMode::Quiet => {
            // Loops forever without writing — isolates whether
            // child-output VOLUME is part of the bug trigger.
            "use std::thread;\n\
             use std::time::Duration;\n\
             \n\
             fn main() {\n\
             \x20\x20\x20\x20loop {\n\
             \x20\x20\x20\x20\x20\x20\x20\x20thread::sleep(Duration::from_secs(1));\n\
             \x20\x20\x20\x20}\n\
             }\n"
                .to_owned()
        }
    }
}

/// Materialise a synthetic workspace at `root` driven by `opts`. The
/// workspace shape (member layout, Cargo.tomls, source files) is
/// fully determined by `opts`; per-binary mod.rs files come from
/// `generate_test_binary_mod_rs`.
fn write_fixture_with_opts(root: &Path, rudzio_root: &Path, opts: &FixtureOpts) -> io::Result<()> {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [{features}] }}\n\
             tokio = {{ version = \"=1.52.3\", features = [\"full\"] }}\n\
             anyhow = \"=1.0.102\"\n",
            rudzio = rudzio_root.display(),
            features = workspace_rudzio_features(opts),
        ),
    )?;

    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join("src").join("bin"))?;
    for bin in &opts.binaries {
        fs::create_dir_all(consumer.join("tests").join(bin.name))?;
    }

    fs::write(
        consumer.join("Cargo.toml"),
        generate_consumer_cargo_toml(opts),
    )?;
    fs::write(
        consumer.join("src").join("lib.rs"),
        generate_consumer_lib_rs(opts),
    )?;

    let needs_child_bin = opts
        .binaries
        .iter()
        .any(|bin| matches!(bin.setup, SetupKind::SpawnChild { .. }))
        || opts
            .binaries
            .iter()
            .any(|bin| matches!(bin.suite_kind, SuiteKind::Shared));
    if needs_child_bin {
        fs::write(
            consumer
                .join("src")
                .join("bin")
                .join("sigkill_repro_child.rs"),
            generate_child_bin_rs(opts),
        )?;
    }

    for bin in &opts.binaries {
        fs::write(
            consumer.join("tests").join(bin.name).join("mod.rs"),
            generate_test_binary_mod_rs(bin),
        )?;
    }

    Ok(())
}

/// Backwards-compat wrapper around `write_fixture_with_opts` that
/// materialises the **primary** Bug #14 repro shape used by the
/// canonical reproduction test.
fn write_fixture(root: &Path, rudzio_root: &Path) -> io::Result<()> {
    write_fixture_with_opts(root, rudzio_root, &FixtureOpts::primary())
}

/// `SIGKILL` the entire process group `pgid`. Best-effort.
#[expect(
    unsafe_code,
    reason = "killpg is the only way to reap grandchild processes (cargo, nested cargo, the synthetic test runner, AND the spawned `sigkill_repro_child` bins) if the test safety bound expires; `Child::kill()` only reaches the immediate child."
)]
fn kill_pgroup(pgid: libc::pid_t) {
    // SAFETY: `libc::killpg` is a thin syscall wrapper, no aliasing or
    // memory requirements. The pgid was created by `process_group(0)`
    // on the spawned cargo-rudzio Command -- we own it.
    let _ret: i32 = unsafe { libc::killpg(pgid, libc::SIGKILL) };
}

/// Query the calling process's process-group id. Wrapper around
/// `libc::getpgid(0)` so the `unsafe` block stays in one well-named
/// helper rather than scattered through P1/P3 orphan-detection tests.
#[expect(
    unsafe_code,
    reason = "libc::getpgid(0) is a thin syscall wrapper; querying our own pgid is unconditionally safe."
)]
fn current_pgid() -> libc::pid_t {
    // SAFETY: `getpgid(0)` queries the calling process's pgid — no
    // aliasing, no memory requirements, never fails for pid 0.
    unsafe { libc::getpgid(0) }
}

// Full 9-runtime matrix mirroring the rudzio-wide convention for
// runtime-spanning tests. When the same spec runs concurrently under
// every runtime, the 9 parallel `cargo-rudzio test` subprocesses
// contend on the outer `CARGO_TARGET_DIR` (specifically the shared
// `<target>/rudzio-auto-runner/Cargo.toml` that each invocation
// rewrites). That contention is itself a bug signal — multiple
// parallel invocations of `cargo-rudzio test` against a single target
// dir MUST NOT clobber each other. Keeping the full matrix here
// surfaces both the primary Bug #14 (SIGKILL/hang at ~80 tests) AND
// the secondary parallel-invocation contention.
#[rudzio::suite([
    (runtime = Multithread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = CurrentThread::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = Local::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = compio::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = embassy::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = ThreadPool::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = async_std::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    (runtime = smol::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = IsolatedCargoSuite, test = IsolatedCargoTest),
])]
mod tests {
    use std::fs;
    use std::io;
    use std::io::Error as IoError;
    use std::os::unix::process::CommandExt as _;
    use std::process::{ExitStatus, Output};
    use std::sync::mpsc;
    use std::thread;

    use rudzio::context::Test as _;

    use super::{
        BinaryOpts, Child, ChildWriteMode, Command, Duration, FixtureOpts, HostRuntime, Instant,
        IsolatedCargoTest, Path, PipeMode, SetupKind, Stdio, SuiteKind, TempDir, current_pgid,
        kill_pgroup, rudzio_source_root, write_fixture, write_fixture_with_opts,
    };

    /// Spawn cargo-rudzio against `$root`, then await the wait-thread's
    /// notification via `$ctx.spawn_blocking(... recv_timeout ...)`. On
    /// timeout, kill the pgroup and await the post-kill output the
    /// same way. Coordinates entirely through runtime-agnostic async
    /// primitives (`Suite::spawn_blocking`) and OS event waits
    /// (`mpsc::recv_timeout` uses a parked-thread futex with
    /// deadline). No polling, no `ctx.sleep`.
    ///
    /// Implemented as a macro (not a generic async fn) so the
    /// concrete `R` runtime parameter from the calling
    /// `#[rudzio::test]` site is used directly -- a free-standing
    /// generic helper would need to declare HRTB-bounded `R` which
    /// the suite macro's emitted runtime types don't always satisfy.
    macro_rules! run_cargo_rudzio_against {
        ($ctx:expr, $root:expr, $extra_args:expr) => {{
            let __root_path: &Path = $root;
            let __extra_args: &[&str] = $extra_args;
            // Hand the cargo-rudzio subprocess the PER-TEST
            // `CARGO_HOME` (hardlinked from the suite's pre-warmed
            // shared one by [`IsolatedCargoSuite::context`]) and a
            // PER-TEST `CARGO_TARGET_DIR` (a fresh empty dir inside
            // the same per-test tempdir). This isolates the test
            // from sibling fixtures on two axes at once:
            //   1. `.package-cache` flock — each test holds its own
            //      flock inside its own `CARGO_HOME`, so the matrix
            //      no longer serialises on a shared lock.
            //   2. aggregator-dir trash — cargo-rudzio writes its
            //      generated aggregator workspace under
            //      `<CARGO_TARGET_DIR>/rudzio-auto-runner-<hash>/`;
            //      directing that to the per-test tempdir means the
            //      whole tree disappears when the test drops, with
            //      no accumulation in the host's target cache.
            let __cargo_home: &Path = $ctx.cargo_home();
            let __cargo_target_dir: &Path = $ctx.cargo_target_dir();
            let (output_rx, pgid) = spawn_cargo_rudzio_with_wait_thread(
                __root_path,
                __cargo_home,
                __cargo_target_dir,
                __extra_args,
            )?;
            let started = Instant::now();
            let (recv_result, output_rx) = $ctx
                .spawn_blocking(move || (output_rx.recv_timeout(SAFETY_BOUND), output_rx))
                .await
                .map_err(|err| anyhow::anyhow!("spawn_blocking join error: {err}"))?;
            let elapsed = started.elapsed();
            let (output, wedged): (Output, bool) = match recv_result {
                Ok(Ok(output)) => (output, false),
                Ok(Err(io_err)) => anyhow::bail!("wait_with_output IO error: {io_err}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    kill_pgroup(pgid);
                    let post_kill: io::Result<Output> = $ctx
                        .spawn_blocking(move || output_rx.recv())
                        .await
                        .map_err(|err| anyhow::anyhow!("spawn_blocking join error: {err}"))?
                        .map_err(|err| anyhow::anyhow!("output channel closed: {err}"))?;
                    match post_kill {
                        Ok(output) => (output, true),
                        Err(io_err) => anyhow::bail!(
                            "wedged then killed; wait_with_output IO error after kill: {io_err}"
                        ),
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("wait-thread channel disconnected before delivering output")
                }
            };
            RunOutcome {
                elapsed,
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                wedged,
            }
        }};
    }

    /// Generous upper bound on the synthetic run. The pre-fix bug
    /// (SIGKILL at ~80 tests) appears within seconds-to-minutes; the
    /// post-fix expected path completes well under this. If the
    /// `recv_timeout` on the wait-thread channel expires first, we
    /// kill the cargo-rudzio process group and treat the run as
    /// wedged. The receiver+timeout is event-driven via futex (not a
    /// polling loop), so the wake-up is immediate when the child
    /// exits — no sleep, no busy-wait.
    const SAFETY_BOUND: Duration = Duration::from_mins(5);

    /// Outcome of one `cargo rudzio test` subprocess invocation.
    struct RunOutcome {
        /// Wall-clock elapsed from spawn to exit (or to wedge kill).
        elapsed: Duration,
        /// Final exit status. Always present -- either from the natural
        /// exit or from the post-kill `wait_with_output()`.
        status: ExitStatus,
        /// Captured stderr (UTF-8 lossy).
        stderr: String,
        /// Captured stdout (UTF-8 lossy).
        stdout: String,
        /// `true` when the wait-thread channel hit `recv_timeout`'s
        /// deadline before the child exited. In that case we killed
        /// the process group via `killpg(SIGKILL)`; the status above
        /// reflects OUR kill, not whatever the bug would have done.
        /// Distinguishes "we wedged-killed" from "external `SIGKILL`".
        wedged: bool,
    }

    impl RunOutcome {
        /// Combined stdout + stderr -- convenience for substring search.
        fn combined(&self) -> String {
            let cap = self
                .stdout
                .len()
                .saturating_add(self.stderr.len())
                .saturating_add(2);
            let mut tmp = String::with_capacity(cap);
            tmp.push_str(&self.stdout);
            tmp.push('\n');
            tmp.push_str(&self.stderr);
            tmp
        }
    }

    /// Spawn `cargo-rudzio test <extra_args>` against the workspace
    /// at `root`. The spawned child's `stdout`/`stderr` are piped; a
    /// detached `std::thread` consumes them via `wait_with_output()`
    /// and posts the result through an `mpsc::Receiver`. The wait is
    /// event-driven (the OS wakes the thread when the child exits);
    /// the channel coordinates that event back to the async runtime.
    ///
    /// Returns `(receiver, pgid)`. The caller awaits the receiver via
    /// `ctx.spawn_blocking(move || receiver.recv_timeout(...))` — a
    /// runtime-agnostic async primitive that dispatches the blocking
    /// recv to a worker thread and returns a Future the rudzio
    /// runtime can poll without a polling loop.
    fn spawn_cargo_rudzio_with_wait_thread(
        root: &Path,
        cargo_home: &Path,
        cargo_target_dir: &Path,
        extra_args: &[&str],
    ) -> io::Result<(mpsc::Receiver<io::Result<Output>>, libc::pid_t)> {
        let cargo_rudzio_bin: &str = env!("CARGO_BIN_EXE_cargo-rudzio");
        // `cargo_home` is per-test (hardlinked from the suite's
        // pre-warmed shared registry by
        // [`IsolatedCargoSuite::context`]) and `cargo_target_dir` is
        // a per-test fresh empty dir. Both live inside the same
        // per-test tempdir, so they're cleaned atomically by test
        // teardown.
        //
        // `CARGO_NET_OFFLINE=1` is the third leg of the isolation
        // story: the pre-warm runs `cargo check` against the
        // registry deps the fixtures need, leaving `registry/index/`,
        // `registry/cache/` and `registry/src/` fully populated.
        // With `--offline` (env-equivalent
        // `CARGO_NET_OFFLINE=1`), cargo never attempts to refresh
        // the index — every fixture resolves from local state
        // without hitting the network or contending for an index
        // update lock.
        let mut command = Command::new(cargo_rudzio_bin);
        let _configured = command
            .arg("test")
            .args(extra_args)
            .current_dir(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", cargo_target_dir)
            .env("CARGO_NET_OFFLINE", "true")
            .process_group(0_i32);
        let child: Child = command.spawn()?;
        let pgid: libc::pid_t = libc::pid_t::try_from(child.id())
            .map_err(|err| IoError::other(format!("pid did not fit in pid_t: {err}")))?;
        let (tx, rx) = mpsc::channel();
        let _join_handle = thread::spawn(move || {
            // `wait_with_output` consumes the child and reads both
            // pipes to EOF -- the kernel wakes this thread exactly
            // when the child exits.
            let _ignore = tx.send(child.wait_with_output());
        });
        Ok((rx, pgid))
    }

    /// Assert the run reached the summary phase (`test result:` line
    /// present) and was NOT killed mid-stream — either by our
    /// safety-bound `killpg` (`wedged == true`) or by an external
    /// SIGKILL (`status.code() == Some(137)` with `wedged == false`).
    /// Shared post-fix invariant across every "should pass" variant.
    fn assert_run_reached_summary_and_not_sigkilled(
        run: &RunOutcome,
        context: &str,
    ) -> anyhow::Result<()> {
        if run.wedged {
            anyhow::bail!(
                "{context}: wedged — `cargo rudzio test` did not exit within {}s. \
                 Killed via `killpg(SIGKILL)` on the test's process group.\n\
                 --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
                SAFETY_BOUND.as_secs(),
                run.stdout,
                run.stderr,
            );
        }
        anyhow::ensure!(
            run.status.code() != Some(137_i32),
            "{context}: cargo-rudzio test exited 137 (SIGKILL by external party) after \
             {:.1?}. The test did NOT wedge from our side, so an external killer (likely \
             systemd-oomd or a watchdog) tore down the runner mid-stream.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.elapsed,
            run.stdout,
            run.stderr,
        );
        anyhow::ensure!(
            run.combined().contains("test result:"),
            "{context}: cargo-rudzio test exited {exit:?} after {:.1?} but emitted no \
             `test result:` summary line. The runner never reached the summary phase.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.elapsed,
            run.stdout,
            run.stderr,
            exit = run.status.code(),
        );
        Ok(())
    }

    /// Bug #14 primary reproduction: `cargo rudzio test` against the
    /// canonical two-binary, child-spawning, piped-and-drained
    /// fixture MUST exit non-137 AND emit a `test result:` summary
    /// line.
    #[rudzio::test]
    async fn cargo_rudzio_test_does_not_sigkill_with_two_child_spawning_test_binaries(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "primary 2-binary repro")
    }

    // ===========================================================
    // Bug #14 DEFENSIVE: P1 single-axis isolation. Each test changes
    // exactly ONE dimension from the primary repro to wedge the
    // bug-trigger between necessary and sufficient.
    // ===========================================================

    /// P1 isolation: single binary, same shape (distinct Suite, full
    /// child spawn + pipe + drain, 100 tests). The bug requires
    /// TWO+ binaries — this control MUST pass cleanly.
    #[rudzio::test]
    async fn single_binary_with_child_spawn_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![BinaryOpts::primary("alpha")],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "single-binary isolation")
    }

    /// P1 isolation: two binaries SHARING one Suite type (imported
    /// from `consumer::shared`). cargo-rudzio's aggregator unifies
    /// them into ONE runtime group → ONE `Suite::setup` call → ONE
    /// child process. The bug requires distinct Suite types — this
    /// control MUST pass cleanly.
    #[rudzio::test]
    async fn two_binaries_sharing_suite_type_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    suite_kind: SuiteKind::Shared,
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    suite_kind: SuiteKind::Shared,
                    ..BinaryOpts::primary("beta")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "shared-suite isolation")
    }

    /// P1 isolation: two binaries with distinct Suites but the Suites
    /// do NOT spawn any child process. Tests whether child-process
    /// presence is part of the trigger.
    #[rudzio::test]
    async fn two_binaries_distinct_suites_no_child_spawn_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    setup: SetupKind::NoChild,
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    setup: SetupKind::NoChild,
                    ..BinaryOpts::primary("beta")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "no-child isolation")
    }

    /// P1 isolation: two binaries with distinct Suites that spawn a
    /// child, but with `Stdio::null()` for stdout/stderr — no pipes
    /// at all. Tests whether the pipe surface is part of the trigger.
    #[rudzio::test]
    async fn two_binaries_distinct_suites_unpiped_child_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    setup: SetupKind::SpawnChild {
                        pipe: PipeMode::Null,
                        drain: false,
                    },
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    setup: SetupKind::SpawnChild {
                        pipe: PipeMode::Null,
                        drain: false,
                    },
                    ..BinaryOpts::primary("beta")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "unpiped-child isolation")
    }

    /// P1 isolation: two binaries with distinct Suites that spawn a
    /// piped child but DO NOT spawn drain tasks. Tests whether the
    /// drain task itself is part of the trigger (vs just the FD
    /// keep-alive from holding pipe handles).
    #[rudzio::test]
    async fn two_binaries_distinct_suites_piped_no_drain_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    setup: SetupKind::SpawnChild {
                        pipe: PipeMode::Piped,
                        drain: false,
                    },
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    setup: SetupKind::SpawnChild {
                        pipe: PipeMode::Piped,
                        drain: false,
                    },
                    ..BinaryOpts::primary("beta")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "piped-no-drain isolation")
    }

    /// P1 isolation: full primary shape but the child binary writes
    /// no output. Tests whether the volume of child output is part
    /// of the trigger (vs the mere presence of piped FDs).
    #[rudzio::test]
    async fn two_binaries_distinct_suites_quiet_child_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![BinaryOpts::primary("alpha"), BinaryOpts::primary("beta")],
            child_writes: ChildWriteMode::Quiet,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "quiet-child isolation")
    }

    // ===========================================================
    // Bug #14 DEFENSIVE: P1 scale variants. The user reports SIGKILL
    // at ~80 tests; vary the count to map the threshold.
    // ===========================================================

    /// P1 scale: 50 tests per binary (100 total). Below the user's
    /// reported ~80-test threshold from one binary — may or may not
    /// reproduce. The test asserts a clean summary; if 50/binary is
    /// below the trigger threshold, the test passes (expected
    /// post-fix); if not, it fails (the bug fires earlier than ~80).
    #[rudzio::test]
    async fn two_binaries_50_tests_each_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    tests: 50,
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    tests: 50,
                    ..BinaryOpts::primary("beta")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "50-tests-per-binary scale")
    }

    /// P1 scale: 200 tests per binary (400 total). Well above the
    /// user's threshold — should reproduce the bug more reliably
    /// pre-fix, and stress-test the post-fix behavior.
    #[rudzio::test]
    async fn two_binaries_200_tests_each_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    tests: 200,
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    tests: 200,
                    ..BinaryOpts::primary("beta")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "200-tests-per-binary scale")
    }

    /// P1 scale: 3 binaries with 67 tests each (201 total). More
    /// runtime groups, more child processes alive simultaneously.
    /// Increases pressure along the runtime-group dimension rather
    /// than the test-count dimension.
    #[rudzio::test]
    async fn three_binaries_distinct_suites_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    tests: 67,
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    tests: 67,
                    ..BinaryOpts::primary("beta")
                },
                BinaryOpts {
                    tests: 67,
                    ..BinaryOpts::primary("gamma")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "3-binary scale")
    }

    // ===========================================================
    // Bug #14 DEFENSIVE: P1 post-fix invariants. Each test runs the
    // primary fixture and asserts a specific aspect of the output.
    // ===========================================================

    /// P1 invariant: after a successful run, no `sigkill_repro_child`
    /// PIDs remain in the parent's process group. Orphan children
    /// surviving teardown is the bug's downstream effect that the
    /// user explicitly called out (reparented to systemd).
    #[rudzio::test]
    async fn successful_run_leaves_no_orphan_child_processes(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "orphan-check primary run")?;
        // After cargo-rudzio test exits, the spawned `sigkill_repro_child`
        // PIDs in OUR test process group should be gone. The child
        // binary has the unique name `sigkill_repro_child`; pgrep
        // filtered to our pgid scopes the check.
        let our_pgid: libc::pid_t = current_pgid();
        let pgrep_out = Command::new("pgrep")
            .args(["-g", &our_pgid.to_string(), "-f", "sigkill_repro_child"])
            .output()?;
        let stdout = String::from_utf8_lossy(&pgrep_out.stdout);
        let orphan_pids: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
        anyhow::ensure!(
            orphan_pids.is_empty(),
            "successful cargo-rudzio test must leave NO `sigkill_repro_child` orphans in \
             our process group. Found {n} orphan PID(s):\n{pids:?}\n\
             --- captured stdout ---\n{out}\n--- captured stderr ---\n{err}",
            n = orphan_pids.len(),
            pids = orphan_pids,
            out = run.stdout,
            err = run.stderr,
        );
        Ok(())
    }

    /// P1 invariant: a successful primary run exits with code 0 (all
    /// 200 tests pass). Pre-fix: never reaches summary (137 or wedge);
    /// post-fix: 0 because all tests are trivial `Ok(())`.
    #[rudzio::test]
    async fn successful_run_exit_code_is_zero(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "exit-code primary run")?;
        anyhow::ensure!(
            run.status.code() == Some(0_i32),
            "primary run with all trivial tests must exit 0; got {:?}.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.status.code(),
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    /// P1 invariant: summary line shows the correct total count for
    /// 200 trivial tests (100 alpha + 100 beta).
    #[rudzio::test]
    async fn successful_run_summary_line_reports_correct_total(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "summary-count primary run")?;
        let combined = run.combined();
        anyhow::ensure!(
            combined.contains("200 total"),
            "summary line must report `200 total` (100 alpha + 100 beta). \n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        anyhow::ensure!(
            combined.contains("200 passed"),
            "summary line must report `200 passed` for the trivial-test fixture.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    /// P1 invariant: both binaries' `setup` lines appear in the run
    /// output. Confirms both runtime groups ran their setup phase
    /// (which is where the long-lived child gets spawned).
    #[rudzio::test]
    async fn successful_run_emits_both_setup_lines(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "setup-lines primary run")?;
        let combined = run.combined();
        anyhow::ensure!(
            combined.contains("setup consumer::alpha::tests"),
            "missing `setup consumer::alpha::tests` line.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        anyhow::ensure!(
            combined.contains("setup consumer::beta::tests"),
            "missing `setup consumer::beta::tests` line.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    /// P1 invariant: both binaries' `teardown` lines appear. Confirms
    /// suite-level teardown ran for each runtime group (which is
    /// where the spawned child gets killed).
    #[rudzio::test]
    async fn successful_run_emits_both_teardown_lines(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "teardown-lines primary run")?;
        let combined = run.combined();
        anyhow::ensure!(
            combined.contains("teardown consumer::alpha::tests"),
            "missing `teardown consumer::alpha::tests` line.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        anyhow::ensure!(
            combined.contains("teardown consumer::beta::tests"),
            "missing `teardown consumer::beta::tests` line.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    /// P1 invariant: the child's `diag N` lines (written to its
    /// stderr) must NOT appear in the runner's output. The drain
    /// tasks consume them. Pre-fix the runner may merge child stderr
    /// into its own output stream.
    #[rudzio::test]
    async fn child_diag_lines_do_not_pollute_runner_output(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "stderr-separation primary run")?;
        // The child writes lines like `diag 10`, `diag 20`, ... to its
        // stderr. The drain task in setup consumes them. If they leak
        // into the runner's output, it would mean the FD was inherited
        // unexpectedly.
        let combined = run.combined();
        anyhow::ensure!(
            !combined.contains("diag 10"),
            "child's `diag N` stderr lines leaked into the runner's output stream — \
             the drain task should have consumed them.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        anyhow::ensure!(
            !combined.contains("alive 10"),
            "child's `alive N` stdout lines leaked into the runner's output — same FD-leak \
             diagnostic.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    // ===========================================================
    // Bug #14 DEFENSIVE: P2 standalone-binary invocations. The user
    // reports `--test alpha` and `--test beta` both work alone; we
    // pin this invariant.
    // ===========================================================

    /// P2: `cargo-rudzio test --test alpha` (single-binary filter)
    /// against the primary fixture passes alone. The user reports
    /// this works in their environment; we lock it in here.
    #[rudzio::test]
    async fn single_binary_filter_test_alpha_passes_alone(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &["--test", "alpha"]);
        assert_run_reached_summary_and_not_sigkilled(&run, "--test alpha filter")?;
        // alpha alone has 100 tests; beta should be filtered out.
        let combined = run.combined();
        anyhow::ensure!(
            combined.contains("100 total") || combined.contains("100 passed"),
            "--test alpha must run exactly 100 tests (alpha's count); summary suggests \
             different count.\n--- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    /// P2: `cargo-rudzio test --test beta` (single-binary filter)
    /// passes alone.
    #[rudzio::test]
    async fn single_binary_filter_test_beta_passes_alone(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &["--test", "beta"]);
        assert_run_reached_summary_and_not_sigkilled(&run, "--test beta filter")?;
        let combined = run.combined();
        anyhow::ensure!(
            combined.contains("100 total") || combined.contains("100 passed"),
            "--test beta must run exactly 100 tests.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    // ===========================================================
    // Bug #14 DEFENSIVE: P2 cross-runtime. Force two genuinely
    // distinct runtime groups by using different host runtimes per
    // binary (instead of relying on distinct Suite types alone).
    // ===========================================================

    /// P2: alpha uses Tokio `Multithread`, beta uses Tokio `CurrentThread`.
    /// Two distinct runtime groups by RUNTIME (not just by Suite
    /// type), exercising a cargo-rudzio aggregation path that
    /// distinct-Suite-on-same-runtime doesn't reach.
    #[rudzio::test]
    async fn cross_runtime_alpha_multithread_beta_currentthread_does_not_sigkill(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        let opts = FixtureOpts {
            binaries: vec![
                BinaryOpts {
                    host_runtime: HostRuntime::Multithread,
                    ..BinaryOpts::primary("alpha")
                },
                BinaryOpts {
                    host_runtime: HostRuntime::CurrentThread,
                    ..BinaryOpts::primary("beta")
                },
            ],
            child_writes: ChildWriteMode::Verbose,
        };
        write_fixture_with_opts(workspace.path(), &rudzio_source_root(), &opts)?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "cross-runtime variant")
    }

    // ===========================================================
    // Bug #14 DEFENSIVE: P3 diagnostic-only. These tests don't gate
    // a fix; they SURFACE resource-usage signals that help identify
    // the root cause when the primary repro fires.
    // ===========================================================

    /// P3 diagnostic: across the primary run, the FD count visible to
    /// the cargo-rudzio test runner must stay bounded. An unbounded
    /// climb across tests would explain a SIGKILL by ulimit /
    /// systemd-oomd. The check is best-effort: we sample the runner's
    /// `/proc/<pid>/fd` count after the run completes — pre-fix the
    /// runner is killed before we can sample, so this test fails the
    /// same way as the primary repro; post-fix it succeeds with a
    /// bounded FD count.
    #[rudzio::test]
    async fn runner_fd_count_stays_bounded(ctx: &IsolatedCargoTest) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "fd-count primary run")?;
        // After the run completes, the test runner (US) should have a
        // bounded FD count — we spawned ONE cargo-rudzio child + a
        // bounded number of pipes. Hard upper bound: 256 FDs. If we
        // exceed this, something is leaking inside the test harness.
        // (The cargo-rudzio subprocess's own FD count is harder to
        // observe post-exit; this test bounds the parent.)
        let self_fd_count = fs::read_dir("/proc/self/fd")?.count();
        anyhow::ensure!(
            self_fd_count < 256,
            "FD count in test runner climbed to {self_fd_count}, above the 256 bound. \
             Probably a leak somewhere in the test harness or the spawned cargo-rudzio.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }

    /// P3 diagnostic: child process count in our process group stays
    /// bounded. With 2 binaries each spawning 1 child, plus the
    /// cargo-rudzio and nested cargo, the count should be small
    /// throughout. Spike + persistence indicates orphan accumulation.
    /// Sampled after the run completes.
    #[rudzio::test]
    async fn child_process_count_in_pgroup_stays_bounded(
        ctx: &IsolatedCargoTest,
    ) -> anyhow::Result<()> {
        let workspace = TempDir::new_in(ctx.suite_tempdir())?;
        write_fixture(workspace.path(), &rudzio_source_root())?;
        let run = run_cargo_rudzio_against!(ctx, workspace.path(), &[]);
        assert_run_reached_summary_and_not_sigkilled(&run, "pgroup-count primary run")?;
        let our_pgid: libc::pid_t = current_pgid();
        let pgrep_out = Command::new("pgrep")
            .args(["-g", &our_pgid.to_string()])
            .output()?;
        let stdout = String::from_utf8_lossy(&pgrep_out.stdout);
        let pid_count = stdout.lines().filter(|line| !line.is_empty()).count();
        // Heuristic bound: post-run there should be a handful of
        // processes (our test runner + maybe its parent shell). A
        // count above 32 suggests orphan accumulation.
        anyhow::ensure!(
            pid_count < 32,
            "process count in our pgroup climbed to {pid_count}, above the 32 bound. \
             Probably orphan accumulation from incomplete cleanup.\n\
             --- captured stdout ---\n{}\n--- captured stderr ---\n{}",
            run.stdout,
            run.stderr,
        );
        Ok(())
    }
}
