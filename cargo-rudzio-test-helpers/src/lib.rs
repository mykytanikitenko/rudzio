//! Rudzio Suite/Test scaffolding for cargo-rudzio integration tests
//! that spawn `cargo` and `cargo-rudzio` subprocesses.
//!
//! [`IsolatedCargoSuite`] is the actual contract: at `setup()` it
//! provisions every shared resource (the suite tempdir + a pre-
//! warmed cargo home that lives inside it). For each test,
//! `context()` then mints **per-test** private cargo state by
//! hardlinking the pre-warmed registry into a fresh per-test
//! cargo home and creating an empty per-test cargo target dir. At
//! `teardown()` the suite drops every resource it allocated — both
//! the suite-level pre-warm and all per-test trees disappear.
//!
//! # Why this exists
//!
//! cargo-rudzio's integration suite spawns `cargo` and
//! `cargo-rudzio` subprocesses from inside test bodies. Two
//! pressure points exist when many fixtures run concurrently:
//!
//! 1. **Cargo's `.package-cache` flock.** Cargo serialises all
//!    operations that touch a `CARGO_HOME`'s registry behind a
//!    single `fs2::FileExt::lock_exclusive` on
//!    `<CARGO_HOME>/registry/.package-cache`. Sharing one
//!    `CARGO_HOME` across the 8-9-runtime × ~dozen-test matrix means
//!    ~99 concurrent `cargo build` invocations all waiting on one
//!    lock — the dominant failure mode at scale.
//! 2. **Aggregator-dir accumulation in the host's
//!    `CARGO_TARGET_DIR`.** cargo-rudzio writes its generated
//!    aggregator workspace to
//!    `<CARGO_TARGET_DIR>/rudzio-auto-runner-<consumer-workspace-hash>/`.
//!    Each fixture has a unique consumer-workspace path → unique
//!    hash → unique aggregator dir → never reused. 99 fixtures = 99
//!    orphaned dirs, ~63 GB.
//!
//! Solution: every test gets its own cargo state, both isolated
//! and ephemeral. [`IsolatedCargoSuite::setup`] pre-warms ONE shared
//! registry (via `cargo check` against a minimal manifest, see
//! [`prepopulate_cargo_home`]); [`IsolatedCargoSuite::context`] then
//! hardlinks that registry's `index/`, `cache/` and `src/` subtrees
//! into a per-test `CARGO_HOME` and creates a fresh per-test
//! `CARGO_TARGET_DIR`, both inside a per-test `TempDir`. Hardlinks
//! share the file inode (no byte-level duplication of the ~hundreds
//! of MB of extracted sources) while keeping the directory entries
//! independent — so each test owns its own `.package-cache` flock
//! and its own aggregator subtree.
//!
//! # Usage from a spec file
//!
//! ```rust,ignore
//! use cargo_rudzio_test_helpers::{IsolatedCargoSuite, IsolatedCargoTest};
//!
//! #[rudzio::suite([
//!     (runtime = Multithread::new,
//!      suite = IsolatedCargoSuite,
//!      test = IsolatedCargoTest),
//!     // ...other runtimes...
//! ])]
//! mod tests {
//!     use super::IsolatedCargoTest;
//!
//!     #[rudzio::test]
//!     async fn spawn_under_per_test_cargo_state(
//!         ctx: &IsolatedCargoTest<'_, _>,
//!     ) -> Result<()> {
//!         let fixture_workspace = ctx.suite_tempdir().join("ws-xyz");
//!         std::fs::create_dir_all(&fixture_workspace)?;
//!         // ...write Cargo.toml under fixture_workspace...
//!         let output = std::process::Command::new(env!("CARGO_BIN_EXE_cargo-rudzio"))
//!             .arg("test")
//!             .current_dir(&fixture_workspace)
//!             .env("CARGO_HOME", ctx.cargo_home())             // per-test, hardlinked
//!             .env("CARGO_TARGET_DIR", ctx.cargo_target_dir()) // per-test, fresh
//!             .env("CARGO_NET_OFFLINE", "true")                // pre-warmed registry is offline-complete
//!             .output()?;
//!         // ...assertions...
//!         Ok(())
//!     }
//! }
//! ```
//!
//! # Custom args
//!
//! `--tmp-dir-base=<path>` (passed through `cargo rudzio test --
//! --tmp-dir-base=/var/mnt/data-cache/tmp ...`) anchors the suite
//! tempdir under `<path>` instead of `std::env::temp_dir()`. The flag
//! is consumed from `Config::unparsed`, leaving rudzio's own parsing
//! untouched.

use std::convert::Infallible;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};
use rudzio::Config;
use rudzio::context;
use rudzio::runtime::Runtime;
use rudzio::tokio_util::sync::CancellationToken;
use rudzio::tokio_util::task::TaskTracker;
use tempfile::TempDir;

/// Files inside `<CARGO_HOME>/registry/` that must NOT be hardlinked
/// across per-test cargo homes. `.package-cache` is cargo's
/// `fs2::FileExt`-backed flock target; sharing the inode across tests
/// would re-introduce the very serialisation the per-test home exists
/// to eliminate. Cargo creates a fresh one in each test's home on
/// first invocation.
const REGISTRY_LOCK_FILES: &[&str] = &[".package-cache", ".package-cache.lock"];

/// Subdir name under `<suite_tempdir>` that holds the immutable
/// consumer-workspace template materialised once during
/// [`IsolatedCargoSuite::setup`]. Per-test copies are produced by
/// full-recursive [`copy_dir_recursive`] under
/// [`IsolatedCargoSuite::context`]. The template is read-only after
/// setup completes — no test mutates it, no cleanup step touches it
/// (suite teardown drops the parent tempdir which removes the
/// template along with everything else).
const TEMPLATE_WORKSPACE_DIRNAME: &str = "consumer-workspace-template";

/// Subdir name under `<test_tempdir>` that holds the per-test
/// consumer workspace materialised by full-recursive copy of the
/// suite's template. Exposed to tests via
/// [`IsolatedCargoTest::consumer_workspace`].
const PER_TEST_WORKSPACE_DIRNAME: &str = "consumer-workspace";

/// Rudzio Suite that allocates a per-suite tempdir + pre-warmed
/// shared cargo home once in `setup()`. For each test, `context()`
/// mints a per-test cargo home (hardlinked from the suite's pre-
/// warm) and a per-test cargo target dir, both inside a per-test
/// tempdir nested under the suite tempdir.
///
/// Resources are owned by the suite (RAII via `TempDir`) so dropping
/// the suite at teardown removes the entire tree atomically,
/// including every per-test subtree the suite minted via
/// `context()`. Tests do not allocate or clean up cargo state
/// themselves — they read paths via [`IsolatedCargoTest::cargo_home`]
/// and [`IsolatedCargoTest::cargo_target_dir`] and let the suite
/// handle the lifecycle.
pub struct IsolatedCargoSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    /// Per-suite cancellation token: child of the runner's root token
    /// so `cargo rudzio test --run-timeout` / SIGINT / SIGTERM
    /// propagate to anything the tests spawned.
    cancel: CancellationToken,
    /// Suite-shared pre-warmed cargo home (lives under
    /// `_suite_tempdir`). Per-test cargo homes are hardlinked from
    /// here; this path itself is never used directly as a
    /// `CARGO_HOME` for the tests' spawned cargo subprocesses.
    cargo_home: PathBuf,
    /// Borrow of the async runtime driving this suite.
    rt: &'suite_context R,
    /// Per-suite temp directory. Per-test tempdirs (with their own
    /// cargo home + cargo target dir) are created under here; tests
    /// can also place per-fixture workspaces here directly via
    /// [`IsolatedCargoTest::suite_tempdir`].
    suite_tempdir_path: PathBuf,
    /// Path to the immutable consumer-workspace template
    /// materialised once during [`Self::setup`]. Lives at
    /// `<suite_tempdir>/consumer-workspace-template/`. Source for
    /// per-test full-recursive copies inside [`Self::context`].
    template_workspace: PathBuf,
    /// Suite-shared task tracker (for the `&Test::tracker` contract).
    tracker: TaskTracker,
    /// Owned tempdir handle: kept inside the suite so its `Drop`
    /// fires at teardown and the whole directory tree is removed
    /// atomically (including every per-test tempdir nested under
    /// it). Underscore-prefixed because nothing reads it directly —
    /// it exists purely for its `Drop` side-effect.
    _suite_tempdir: TempDir,
}

impl<'suite_context, R> fmt::Debug for IsolatedCargoSuite<'suite_context, R>
where
    R: Runtime<'suite_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IsolatedCargoSuite")
            .field("suite_tempdir_path", &self.suite_tempdir_path)
            .field("cargo_home", &self.cargo_home)
            .field("template_workspace", &self.template_workspace)
            .finish_non_exhaustive()
    }
}

impl<'suite_context, R> context::Suite<'suite_context, R> for IsolatedCargoSuite<'suite_context, R>
where
    R: for<'any> Runtime<'any> + Sync,
{
    type ContextError = anyhow::Error;
    type SetupError = anyhow::Error;
    type TeardownError = Infallible;
    type Test<'test_context>
        = IsolatedCargoTest<'test_context, R>
    where
        Self: 'test_context;

    #[inline]
    fn cancel_token(&self) -> Result<&CancellationToken> {
        Ok(&self.cancel)
    }

    async fn context<'test_context>(
        &'test_context self,
        cancel: CancellationToken,
        config: &'test_context Config,
    ) -> Result<Self::Test<'test_context>, Self::ContextError> {
        let test_tempdir = TempDir::new_in(&self.suite_tempdir_path).with_context(|| {
            format!(
                "creating per-test tempdir under {}",
                self.suite_tempdir_path.display()
            )
        })?;
        let test_root = test_tempdir.path().to_path_buf();
        let cargo_home = test_root.join("cargo-home");
        hardlink_cargo_home(&self.cargo_home, &cargo_home).with_context(|| {
            format!(
                "hardlinking shared cargo home {} into per-test cargo home {}",
                self.cargo_home.display(),
                cargo_home.display()
            )
        })?;
        let cargo_target_dir = test_root.join("cargo-target");
        fs::create_dir_all(&cargo_target_dir).with_context(|| {
            format!(
                "creating per-test cargo target dir at {}",
                cargo_target_dir.display()
            )
        })?;
        // Phase B per-test copy: every test starts from an
        // independent copy of the suite-warmed consumer workspace
        // template. Tests mutate the copy freely; the per-test
        // tempdir's `Drop` removes it at test teardown.
        // `copy_dir_recursive` is a plain byte-level copy (no
        // hardlinks, no reflink) — slowest but maximally isolated;
        // matches the user's choice for the snapshot/clone
        // mechanism. The template itself is read-only after suite
        // setup, so the copy direction is well-defined.
        let consumer_workspace = test_root.join(PER_TEST_WORKSPACE_DIRNAME);
        copy_dir_recursive(&self.template_workspace, &consumer_workspace).with_context(|| {
            format!(
                "copying consumer workspace template {} -> {}",
                self.template_workspace.display(),
                consumer_workspace.display()
            )
        })?;
        // FIXME(rs-cleanup): callee returns Result, add ?
        Ok(IsolatedCargoTest::new(
            IsolatedCargoTestRuntimeCtx {
                cancel,
                config,
                rt: self.rt,
                tracker: self.tracker.clone(),
            },
            IsolatedCargoTestPaths {
                cargo_home,
                cargo_target_dir,
                consumer_workspace,
                suite_tempdir: &self.suite_tempdir_path,
                test_tempdir,
            },
        ))
    }

    #[inline]
    fn rt(&self) -> Result<&'suite_context R> {
        Ok(self.rt)
    }

    async fn setup(
        rt: &'suite_context R,
        cancel: CancellationToken,
        config: &'suite_context Config,
    ) -> Result<Self, Self::SetupError> {
        // FIXME(rs-cleanup): callee returns Result, add ?
        let tmp_base = resolve_tmp_base(&config.unparsed);
        fs::create_dir_all(&tmp_base).with_context(|| {
            format!(
                "creating tmp base dir for IsolatedCargoSuite at {}",
                tmp_base.display()
            )
        })?;
        let suite_tempdir = TempDir::new_in(&tmp_base)
            .with_context(|| format!("creating per-suite tempdir under {}", tmp_base.display()))?;
        let suite_tempdir_path = suite_tempdir.path().to_path_buf();
        let cargo_home = suite_tempdir_path.join("cargo-home");
        fs::create_dir_all(&cargo_home)
            .with_context(|| format!("creating shared cargo home at {}", cargo_home.display()))?;
        // Per-suite prepop (one cold `cargo check` per
        // `(runtime, suite_type)` tuple): the resulting wall-clock cost
        // is the primary throttling mechanism that staggers when each
        // suite's tests actually start. A global single prepop is
        // faster but releases every suite simultaneously, producing a
        // synchronized compile burst on the synthetic fixtures that
        // pushes wall-clock past the SAFETY_BOUND contract under
        // matrix-scale parallelism. The natural per-suite staggering
        // keeps host CPU saturation gradual.
        prepopulate_cargo_home(&cargo_home, &suite_tempdir_path)
            .context("pre-populating shared cargo home for IsolatedCargoSuite")?;
        // Phase B warmup: materialise the consumer-workspace
        // template once per suite instance. Per-test copies happen
        // inside `context()`. The template's content is invariant
        // across suites (a workspace with one `consumer` member
        // carrying a trivial `#[rudzio::test]`) so every fixture
        // that today hand-rolls this boilerplate can opt in by
        // reading `ctx.consumer_workspace()` instead.
        let template_workspace = suite_tempdir_path.join(TEMPLATE_WORKSPACE_DIRNAME);
        materialize_consumer_workspace_template(&template_workspace).with_context(|| {
            format!(
                "materialising consumer-workspace template at {}",
                template_workspace.display()
            )
        })?;
        Ok(Self {
            cancel: cancel.child_token(),
            cargo_home,
            rt,
            suite_tempdir_path,
            template_workspace,
            // FIXME(rs-cleanup): callee returns Result, add ?
            tracker: TaskTracker::new(),
            _suite_tempdir: suite_tempdir,
        })
    }

    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        self.cancel.cancel();
        let _closed: bool = self.tracker.close();
        self.tracker.wait().await;
        // `_suite_tempdir` drops here, removing the entire per-suite
        // tree (suite_tempdir + cargo_home + any per-fixture
        // workspaces the tests placed under it). The teardown is
        // atomic from the suite's perspective: by the time this
        // function returns, the disk footprint is gone.
        Ok(())
    }

    #[inline]
    fn tracker(&self) -> Result<&TaskTracker> {
        Ok(&self.tracker)
    }
}

/// Per-test view of [`IsolatedCargoSuite`]'s pre-warmed resources,
/// extended with per-test private cargo state.
///
/// Each test gets:
///
/// * `cargo_home()` — a **per-test** `CARGO_HOME` whose `registry/`
///   subtree is hardlinked from the suite-shared pre-warmed one (so
///   the .crate archives, extracted sources and registry index are
///   physically shared via inode) but whose `.package-cache` flock
///   file is absent at creation time. Cargo creates a fresh
///   `.package-cache` in each test's home on first invocation, so
///   sibling tests acquire **independent** flocks instead of
///   serialising on one shared lock — the root cause of the previous
///   matrix-scale wedge.
/// * `cargo_target_dir()` — a **per-test** empty `CARGO_TARGET_DIR`.
///   Set this on spawned cargo subprocesses so cargo-rudzio's
///   generated aggregator workspace lands inside the per-test
///   tempdir, not in the host's shared target tree (where 99 fixture
///   runs previously accumulated ~63 GB of orphaned
///   `rudzio-auto-runner-<hash>/` dirs). When the test drops, the
///   tempdir's `Drop` removes everything atomically.
/// * `consumer_workspace()` — a **per-test** full-recursive copy of
///   the suite's consumer-workspace template. Tests mutate the copy
///   freely without affecting siblings or the template; the per-test
///   tempdir's `Drop` removes the copy at teardown. Migration target
///   for every spec that hand-rolls a `write_fixture(root, ...)` —
///   the template already provides the workspace + consumer member
///   + a trivial `#[rudzio::test]` gating the aggregator build.
/// * `suite_tempdir()` — the per-suite tempdir, exposed for any
///   fixture that wants a workspace location outside its own per-
///   test tempdir (kept for back-compat with existing specs).
#[non_exhaustive]
pub struct IsolatedCargoTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Per-test cancellation token (child of the suite's).
    cancel: CancellationToken,
    /// Per-test `CARGO_HOME` (owned; lives inside `_test_tempdir`).
    /// Its `registry/` is hardlinked from the suite-shared pre-warmed
    /// cargo home; its `.package-cache` flock is created fresh by
    /// cargo on first invocation, isolating this test from siblings.
    cargo_home: PathBuf,
    /// Per-test empty `CARGO_TARGET_DIR` (owned; lives inside
    /// `_test_tempdir`). Aggregator workspaces and compiled
    /// artefacts land here and vanish when the test tempdir drops.
    cargo_target_dir: PathBuf,
    /// Resolved CLI/env configuration.
    config: &'test_context Config,
    /// Per-test consumer workspace (owned; lives inside
    /// `_test_tempdir`). Full-recursive copy of the suite's
    /// consumer-workspace template, materialised by
    /// [`IsolatedCargoSuite::context`] before this struct is
    /// constructed. Tests mutate freely; teardown removes via the
    /// tempdir's `Drop`.
    consumer_workspace: PathBuf,
    /// Borrow of the async runtime driving this test.
    rt: &'test_context R,
    /// Per-suite tempdir (borrowed from suite); tests can create
    /// per-fixture workspaces under here too if they don't want them
    /// inside the per-test tempdir.
    suite_tempdir: &'test_context Path,
    /// Suite-shared task tracker.
    tracker: TaskTracker,
    /// Owned per-test tempdir handle: `Drop` removes the entire
    /// `cargo_home` + `cargo_target_dir` + `consumer_workspace`
    /// tree when the test drops. Underscore-prefixed because
    /// nothing reads it directly — it exists purely for its `Drop`
    /// side-effect.
    _test_tempdir: TempDir,
}

/// Runtime + reporter bundle passed into [`IsolatedCargoTest::new`].
/// Lets the constructor keep its argument count tractable.
#[non_exhaustive]
pub struct IsolatedCargoTestRuntimeCtx<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Per-test cancellation token (child of the suite's).
    pub cancel: CancellationToken,
    /// Resolved CLI/env configuration borrowed from the suite.
    pub config: &'test_context Config,
    /// Borrow of the async runtime driving this test.
    pub rt: &'test_context R,
    /// Suite-shared task tracker.
    pub tracker: TaskTracker,
}

/// Filesystem-paths bundle passed into [`IsolatedCargoTest::new`].
/// Lets the constructor keep its argument count tractable.
#[non_exhaustive]
pub struct IsolatedCargoTestPaths<'test_context> {
    /// Per-test `CARGO_HOME`, hardlinked from the suite-shared pre-warm.
    pub cargo_home: PathBuf,
    /// Per-test empty `CARGO_TARGET_DIR`.
    pub cargo_target_dir: PathBuf,
    /// Per-test consumer workspace, full-recursive copied from the
    /// suite's template.
    pub consumer_workspace: PathBuf,
    /// Per-suite tempdir, borrowed for the test's lifetime.
    pub suite_tempdir: &'test_context Path,
    /// Owned per-test tempdir handle whose `Drop` removes the entire
    /// per-test cargo state at test teardown.
    pub test_tempdir: TempDir,
}

impl<'test_context, R> IsolatedCargoTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    /// Constructor: only the suite's `context()` builds these.
    #[inline]
    #[must_use]
    pub fn new(
        rt_ctx: IsolatedCargoTestRuntimeCtx<'test_context, R>,
        paths: IsolatedCargoTestPaths<'test_context>,
    ) -> Result<Self> {
        let IsolatedCargoTestRuntimeCtx {
            cancel,
            config,
            rt,
            tracker,
        } = rt_ctx;
        let IsolatedCargoTestPaths {
            suite_tempdir,
            cargo_home,
            cargo_target_dir,
            consumer_workspace,
            test_tempdir,
        } = paths;
        Ok(Self {
            cancel,
            cargo_home,
            cargo_target_dir,
            config,
            consumer_workspace,
            rt,
            suite_tempdir,
            tracker,
            _test_tempdir: test_tempdir,
        })
    }

    /// Path to the **per-test** `CARGO_HOME`. Pass as
    /// `.env("CARGO_HOME", ctx.cargo_home())` on every cargo
    /// subprocess the test spawns. Sibling tests have independent
    /// `.package-cache` flocks here — no inter-test serialisation.
    #[inline]
    #[must_use]
    pub fn cargo_home(&self) -> Result<&Path> {
        Ok(&self.cargo_home)
    }

    /// Path to the **per-test** empty `CARGO_TARGET_DIR`. Pass as
    /// `.env("CARGO_TARGET_DIR", ctx.cargo_target_dir())` on every
    /// cargo subprocess the test spawns. cargo-rudzio's aggregator
    /// workspace (and the inner cargo's per-aggregator target tree)
    /// lands inside the test tempdir; teardown removes it atomically.
    #[inline]
    #[must_use]
    pub fn cargo_target_dir(&self) -> Result<&Path> {
        Ok(&self.cargo_target_dir)
    }

    /// Path to the **per-test** consumer workspace materialised by
    /// full-recursive copy of the suite's template at
    /// [`IsolatedCargoSuite::context`]. Workspace layout:
    ///
    /// ```text
    /// <consumer_workspace>/
    /// ├── Cargo.toml          (workspace + workspace.dependencies)
    /// └── consumer/
    ///     ├── Cargo.toml      ([lib] only)
    ///     └── src/
    ///         └── lib.rs      (1 #[rudzio::test])
    /// ```
    ///
    /// Tests may mutate any file under this path (write additional
    /// members, append `[[bin]]` entries, replace `lib.rs`, etc.) —
    /// the per-test copy is independent of siblings and of the
    /// suite template. The per-test tempdir's `Drop` removes the
    /// entire copy at teardown.
    #[inline]
    #[must_use]
    pub fn consumer_workspace(&self) -> Result<&Path> {
        Ok(&self.consumer_workspace)
    }

    /// Path to the per-suite temp directory. Kept for back-compat:
    /// existing specs place per-fixture workspaces under here. New
    /// specs are free to place them under the per-test tempdir
    /// instead (e.g. as a child of `cargo_target_dir()`'s parent).
    #[inline]
    #[must_use]
    pub const fn suite_tempdir(&self) -> &Path {
        self.suite_tempdir
    }
}

impl<'test_context, R> fmt::Debug for IsolatedCargoTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IsolatedCargoTest")
            .field("suite_tempdir", &self.suite_tempdir)
            .field("cargo_home", &self.cargo_home)
            .field("cargo_target_dir", &self.cargo_target_dir)
            .field("consumer_workspace", &self.consumer_workspace)
            .finish_non_exhaustive()
    }
}

impl<'test_context, R> context::Test<'test_context, R> for IsolatedCargoTest<'test_context, R>
where
    R: Runtime<'test_context> + Sync,
{
    type TeardownError = Infallible;

    #[inline]
    fn cancel_token(&self) -> Result<&CancellationToken> {
        Ok(&self.cancel)
    }

    #[inline]
    fn config(&self) -> Result<&Config> {
        Ok(self.config)
    }

    #[inline]
    fn rt(&self) -> Result<&'test_context R> {
        Ok(self.rt)
    }

    #[inline]
    async fn teardown(self, _cancel: CancellationToken) -> Result<(), Self::TeardownError> {
        // The per-test view owns nothing the suite doesn't already
        // clean up at its own teardown. Tests are responsible for
        // their own per-fixture artefacts (workspaces under
        // `suite_tempdir`) — but those land inside the suite-owned
        // tempdir, so suite teardown handles them.
        Ok(())
    }

    #[inline]
    fn tracker(&self) -> Result<&TaskTracker> {
        Ok(&self.tracker)
    }
}

/// Resolve the parent directory under which the per-suite tempdir
/// will live.
///
/// Reads `--tmp-dir-base=<path>` from rudzio's unparsed-arg
/// passthrough (`cargo rudzio test -- --tmp-dir-base=<path> ...`);
/// falls back to `std::env::temp_dir()` when no override is supplied.
/// The flag is consumed silently from the unparsed list — rudzio's
/// runner already warns about unrecognised flags, so the spec layer
/// doesn't need to re-flag them.
fn resolve_tmp_base(unparsed: &[String]) -> Result<PathBuf> {
    for arg in unparsed {
        if let Some(value) = arg.strip_prefix("--tmp-dir-base=")
            && !value.is_empty()
        {
            return Ok(PathBuf::from(value));
        }
    }
    Ok(env::temp_dir())
}

/// Pre-populate `cargo_home` with the registry crates every
/// cargo-rudzio integration fixture pulls in — extracted, indexed,
/// and ready for offline use.
///
/// Strategy: write a minimal `Cargo.toml` listing the registry deps
/// every fixture transitively needs (`anyhow`, `tokio` with
/// `features = ["full"]`), then run `cargo check --offline=false`
/// against it with `CARGO_HOME=<cargo_home>` and a throwaway
/// `CARGO_TARGET_DIR` inside `<work_root>`. The `cargo check` run is
/// what makes pre-warming actually do its job:
///
/// * `cargo fetch` alone would only download `.crate` archives into
///   `<cargo_home>/registry/cache/`. The first fixture to compile
///   would still have to **extract** those archives into
///   `<cargo_home>/registry/src/` while holding cargo's
///   `.package-cache` flock — exactly the serialisation point this
///   pre-warm exists to eliminate.
/// * `cargo check` runs the resolver, downloads (if needed),
///   **extracts** sources into `registry/src/`, then compiles the
///   minimal prepop crate against them. Sibling fixtures that later
///   point `CARGO_HOME` here read from an already-populated registry
///   and need to hold the flock for no longer than a metadata
///   refresh.
///
/// The prepop workspace lives at `<work_root>/prepop/` — a sibling of
/// `cargo_home` inside the same per-suite tempdir, so the throwaway
/// `target/` and `Cargo.lock` get removed when the suite tempdir
/// drops.
///
/// # Errors
///
/// Returns an error when the prepop manifest or `src/lib.rs` cannot
/// be written, or when `cargo check` exits non-zero (with stdout /
/// stderr included for triage).
fn prepopulate_cargo_home(cargo_home: &Path, work_root: &Path) -> Result<()> {
    let prepop_dir = work_root.join("prepop");
    let src_dir = prepop_dir.join("src");
    fs::create_dir_all(&src_dir)
        .with_context(|| format!("creating prepop workspace at {}", prepop_dir.display()))?;
    // Locate the rudzio workspace root: this helper crate's manifest
    // lives at `<workspace>/cargo-rudzio-test-helpers/Cargo.toml`, so
    // its parent dir is the rudzio workspace root. `env!` resolves at
    // compile-time of THIS crate, which is always built from inside
    // the rudzio workspace (the only consumer is cargo-rudzio's own
    // integration tests).
    //
    // We depend on rudzio as a path-dep so the prepop's `cargo check`
    // resolves rudzio's full transitive dep graph (crossbeam-channel,
    // signal-hook, linkme, etc.) — not just the two registry deps
    // that the synthetic fixture's own manifest names directly. Both
    // `runtime-tokio-multi-thread` and `runtime-tokio-current-thread`
    // are enabled to cover the union of features any spec's
    // synthetic fixture may activate (see
    // `bug_14_monolith_sigkill_spec::workspace_rudzio_features`).
    // FIXME(rs-cleanup): callee returns Result, add ?
    let rudzio_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cargo-rudzio-test-helpers has no parent dir; expected to live as a workspace \
                 member at <workspace>/cargo-rudzio-test-helpers/"
            )
        })?
        .to_path_buf();
    fs::write(
        prepop_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "rudzio-test-prepop"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
anyhow = "=1.0.102"
tokio = {{ version = "=1.52.3", features = ["full"] }}
rudzio = {{ path = "{rudzio_root}", features = ["common", "runtime-tokio-multi-thread", "runtime-tokio-current-thread"] }}
"#,
            rudzio_root = rudzio_root.display(),
        ),
    )
    .with_context(|| format!("writing prepop Cargo.toml under {}", prepop_dir.display()))?;
    fs::write(src_dir.join("lib.rs"), "")
        .with_context(|| format!("writing prepop src/lib.rs under {}", src_dir.display()))?;
    let prepop_target = work_root.join("prepop-target");
    let cargo_bin = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // Pin every env var the prepop's resolver depends on, regardless of
    // what concurrent sibling tests may have set in the parent process.
    // Other specs (registry_preservation_spec, cfg_test_warning_*, etc.)
    // mutate `CARGO_NET_OFFLINE` via `env::set_var` for their own in-
    // process `cargo metadata` calls — process-wide mutation that races
    // with this subprocess spawn. Explicit `false` here keeps the
    // prepop's first-call cold compile reachable regardless of when
    // those sibling guards fire.
    // FIXME(rs-cleanup): callee returns Result, add ?
    let output = Command::new(&cargo_bin)
        .arg("check")
        .arg("--manifest-path")
        .arg(prepop_dir.join("Cargo.toml"))
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TARGET_DIR", &prepop_target)
        .env("CARGO_NET_OFFLINE", "false")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| {
            format!(
                "spawning `cargo check` to pre-populate cargo home at {}",
                cargo_home.display()
            )
        })?;
    anyhow::ensure!(
        output.status.success(),
        "cargo check pre-populating cargo home at {} exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        cargo_home.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

/// Materialise the consumer-workspace template at `dest`. Called
/// exactly once per [`IsolatedCargoSuite`] instance, from
/// [`IsolatedCargoSuite::setup`].
///
/// Layout written:
///
/// ```text
/// <dest>/
/// ├── Cargo.toml          (workspace + workspace.dependencies)
/// └── consumer/
///     ├── Cargo.toml      ([lib] only, rudzio + anyhow dev-deps)
///     └── src/
///         └── lib.rs      (1 #[rudzio::test])
/// ```
///
/// The single rudzio surface ensures the workspace produces an
/// aggregator when cargo-rudzio runs against it — without at least
/// one `#[rudzio::test]`, cargo-rudzio emits no aggregator subdir
/// and downstream tests have nothing to assert against. The
/// `rudzio` workspace.dependency points at the rudzio repo this
/// helper crate lives inside (env!("CARGO_MANIFEST_DIR") of this
/// crate, parent dir = rudzio root), so the template exercises the
/// rudzio code under test rather than a published version.
///
/// `features` enabled on the workspace `rudzio` entry are the
/// union of what existing fixtures' hand-rolled workspaces enable
/// (`common` + the two tokio runtimes). Tests that need additional
/// rudzio features rewrite this `Cargo.toml` after the per-test
/// copy lands — the template is the baseline, not a hard ceiling.
///
/// # Errors
///
/// Returns an error when any `fs::write` or `fs::create_dir_all`
/// call fails, or when the helper crate has no parent dir (the
/// `CARGO_MANIFEST_DIR` ancestry-walk should never fail in a
/// correctly-checked-out workspace; the explicit error surfaces if
/// it does).
fn materialize_consumer_workspace_template(dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("creating template workspace dir at {}", dest.display()))?;
    // FIXME(rs-cleanup): callee returns Result, add ?
    let rudzio_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cargo-rudzio-test-helpers has no parent dir; expected to live as a workspace \
                 member at <workspace>/cargo-rudzio-test-helpers/"
            )
        })?
        .to_path_buf();
    fs::write(
        dest.join("Cargo.toml"),
        format!(
            "[workspace]\n\
             members = [\"consumer\"]\n\
             resolver = \"3\"\n\
             \n\
             [workspace.package]\n\
             edition = \"2024\"\n\
             \n\
             [workspace.dependencies]\n\
             rudzio = {{ path = \"{rudzio}\", features = [\"common\", \"runtime-tokio-multi-thread\", \"runtime-tokio-current-thread\"] }}\n\
             anyhow = \"=1.0.102\"\n",
            rudzio = rudzio_root.display(),
        ),
    )
    .with_context(|| format!("writing template workspace Cargo.toml under {}", dest.display()))?;
    let consumer_dir = dest.join("consumer");
    let consumer_src = consumer_dir.join("src");
    fs::create_dir_all(&consumer_src).with_context(|| {
        format!(
            "creating consumer src dir at {}",
            consumer_src.display()
        )
    })?;
    fs::write(
        consumer_dir.join("Cargo.toml"),
        "[package]\n\
         name = \"consumer\"\n\
         version = \"0.0.0\"\n\
         edition = { workspace = true }\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"consumer\"\n\
         path = \"src/lib.rs\"\n\
         \n\
         [dev-dependencies]\n\
         rudzio = { workspace = true }\n\
         anyhow = { workspace = true }\n",
    )
    .with_context(|| {
        format!(
            "writing consumer Cargo.toml at {}",
            consumer_dir.join("Cargo.toml").display()
        )
    })?;
    fs::write(
        consumer_src.join("lib.rs"),
        "// rudzio_test marker\n\
         #[cfg(any(test, rudzio_test))]\n\
         mod tests {\n\
         \x20\x20\x20\x20use rudzio::common::context::{Suite, Test};\n\
         \x20\x20\x20\x20use rudzio::runtime::tokio::Multithread as TokioMultithread;\n\
         \n\
         \x20\x20\x20\x20#[rudzio::suite([(runtime = TokioMultithread::new, suite = Suite, test = Test)])]\n\
         \x20\x20\x20\x20mod inner {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20#[rudzio::test]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20fn trivial_test(_t: &super::Test) -> Result<()> {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20Ok(())\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}\n\
         \x20\x20\x20\x20}\n\
         }\n",
    )
    .with_context(|| {
        format!(
            "writing consumer lib.rs at {}",
            consumer_src.join("lib.rs").display()
        )
    })?;
    Ok(())
}

/// Full-recursive byte-level copy of `src` to `dst`. Used by
/// [`IsolatedCargoSuite::context`] to materialise a per-test copy
/// of the consumer-workspace template.
///
/// Trade-off chosen: plain `fs::copy` (which on Linux uses
/// `copy_file_range` and the kernel may opt into reflink/CoW
/// transparently when source and destination share a CoW-capable
/// filesystem like btrfs/xfs/zfs/apfs — purely a kernel-level
/// optimisation that does not change the userspace semantics). The
/// userspace contract is independent per-test copies; any concrete
/// space-sharing the FS layer provides is a bonus.
///
/// Symlinks inside `src` are bailed on (the template never produces
/// any; if a future template adds symlinks, this fn needs the
/// `symlink_metadata` + `symlink` branch).
///
/// # Errors
///
/// Returns the first I/O error encountered when reading `src`,
/// creating `dst` subdirectories, or copying a file. Symlinks
/// inside `src` produce an explicit "unsupported" error rather
/// than silent skip so a future template change doesn't silently
/// lose files.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in
        fs::read_dir(src).with_context(|| format!("reading source dir {}", src.display()))?
    {
        let entry =
            entry.with_context(|| format!("iterating dir entry under {}", src.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat'ing dir entry {}", entry.path().display()))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "copying {} -> {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        } else {
            anyhow::bail!(
                "copy_dir_recursive: unsupported entry type at {} (only regular files and \
                 directories are handled; symlinks/sockets/devices would silently drop bytes)",
                src_path.display(),
            );
        }
    }
    Ok(())
}

/// Materialise a per-test `CARGO_HOME` at `dst` by hardlinking the
/// pre-populated `registry/` subtree from `src` (index, cache,
/// extracted sources) without copying the bytes.
///
/// **Why hardlinks, not copies or symlinks**:
///
/// * `fs::copy` would multiply disk usage by N (one copy per test ×
///   ~hundreds of MB of extracted registry sources).
/// * `symlink_dir` would route every test's `<cargo_home>/registry`
///   access back to the suite-shared directory inode — including the
///   `.package-cache` flock that lives **inside** `registry/`,
///   re-serialising sibling tests on one lock.
/// * `hard_link` shares the file inode for static registry payloads
///   (extracted source files never change post-install) while
///   leaving the directory entries independent, so each per-test
///   `registry/` can carry its own `.package-cache` created freshly
///   by cargo on first invocation.
///
/// Files in [`REGISTRY_LOCK_FILES`] are deliberately NOT hardlinked
/// from the source registry — they're cargo's flock targets. Each
/// per-test home must own its own.
///
/// # Errors
///
/// Returns an error when the destination cannot be created or when
/// any individual `hard_link` call fails (e.g. cross-filesystem —
/// the suite tempdir and the source cargo home must live on the
/// same filesystem). Loud failure on first error: a partially-
/// hardlinked cargo home would silently mis-share inodes.
fn hardlink_cargo_home(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("creating per-test cargo home dir at {}", dst.display()))?;
    let src_registry = src.join("registry");
    if !src_registry.is_dir() {
        anyhow::bail!(
            "expected pre-populated `registry/` subdir at {} (suite pre-warm should have created it); \
             refusing to mint a per-test cargo home from an unpopulated source",
            src_registry.display(),
        );
    }
    let dst_registry = dst.join("registry");
    hardlink_dir_recursive(&src_registry, &dst_registry).with_context(|| {
        format!(
            "hardlinking registry subtree {} → {}",
            src_registry.display(),
            dst_registry.display()
        )
    })?;
    Ok(())
}

/// Recursive hardlink: mirror `src`'s directory shape at `dst`,
/// `fs::hard_link` for every regular file. Skips files whose name
/// appears in [`REGISTRY_LOCK_FILES`] (cargo flock targets — those
/// must be per-test, not shared).
///
/// Symlinks inside `src` are skipped silently; a vanilla
/// pre-populated cargo registry doesn't contain any (cargo extracts
/// `.crate` tarballs into plain files), so silent-skip is the
/// minimal-scope choice that avoids a `fs::read_link` /
/// `symlink_metadata` branch the registry layout never exercises.
///
/// # Errors
///
/// Returns the first I/O error encountered when reading `src`,
/// creating `dst` subdirectories, or hardlinking a file. Aborts the
/// walk on first error (no partial-tree completion).
fn hardlink_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in
        fs::read_dir(src).with_context(|| format!("reading source dir {}", src.display()))?
    {
        let entry =
            entry.with_context(|| format!("iterating dir entry under {}", src.display()))?;
        let file_name = entry.file_name();
        if let Some(name_str) = file_name.to_str()
            && REGISTRY_LOCK_FILES.contains(&name_str)
        {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat'ing dir entry {}", entry.path().display()))?;
        let src_path = entry.path();
        let dst_path = dst.join(&file_name);
        if file_type.is_dir() {
            hardlink_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::hard_link(&src_path, &dst_path).with_context(|| {
                format!(
                    "hardlinking {} → {} (same-filesystem required)",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
        // Symlinks: skipped (vanilla cargo registries don't contain any).
    }
    Ok(())
}
