#![allow(
    unused_results,
    clippy::needless_pass_by_value,
    reason = "toml_edit's insert/push API routinely returns the previous value; CLI glue does not care about the dropped option"
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::{Result as FmtResult, Write as _};
use std::fs;
use std::io::Result as IoResult;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use cargo_metadata::camino::Utf8PathBuf;
use cargo_metadata::{Metadata, MetadataCommand, Package, TargetKind};
use syn::meta::ParseNestedMeta;
use syn::spanned::Spanned as _;
use syn::visit_mut::{self, VisitMut};
use toml_edit::{Array, DocumentMut, Formatted, InlineTable, Item, Table, Value, value};

/// Crate name of the aggregator package this module emits.
const AGGREGATOR_NAME: &str = "rudzio-auto-runner";
/// Verbatim helper code embedded into generated `build.rs` files for bin-bearing members.
///
/// The aggregator and bridge crates inline this string into their
/// emitted `build.rs` so they can shell out to `cargo build --bins`
/// against the real member's manifest and re-export the resulting
/// binary paths via `CARGO_BIN_EXE_<bin>` env vars.
const BUILD_RS_HELPERS: &str = r#"use std::path::PathBuf;
use std::process::Command;

fn env_var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("env var `{name}` not set by cargo"))
}

fn expose_member_bins(pkg: &str, manifest_dir: &str, bins: &[&str]) -> Result<(), String> {
    let out_dir = PathBuf::from(env_var("OUT_DIR")?);
    let profile = env_var("PROFILE")?;
    let cargo = std::env::var_os("CARGO").ok_or_else(|| "CARGO env var not set".to_owned())?;
    let target_dir = out_dir.join("rudzio-auto-bin-cache").join(pkg);

    let manifest = PathBuf::from(manifest_dir).join("Cargo.toml");
    let mut cmd = Command::new(&cargo);
    cmd.arg("build")
        .arg("--bins")
        .arg("--manifest-path")
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_dir)
        // Override `[build] build-dir = "..."` inherited from any
        // outer `.cargo/config.toml`. Without this, the nested cargo
        // contends with the outer cargo (or grandparent cargo) on a
        // single shared `<build-dir>/.cargo-lock` flock and deadlocks
        // forever (cargo file locks have no timeout). Point it at the
        // same isolated per-OUT_DIR cache as CARGO_TARGET_DIR so the
        // nested cargo's intermediate artefacts are scoped to this
        // build.rs invocation alone.
        .env("CARGO_BUILD_BUILD_DIR", &target_dir)
        // rudzio's panic isolation depends on `catch_unwind`, which
        // only works under `panic = "unwind"`. A user-global config
        // may set `panic = "abort"`; these env vars (highest cargo
        // precedence) force unwind for the consumer binaries we're
        // about to build.
        .env("CARGO_PROFILE_DEV_PANIC", "unwind")
        .env("CARGO_PROFILE_RELEASE_PANIC", "unwind");
    if profile == "release" {
        cmd.arg("--release");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("spawning `cargo build --bins` for `{pkg}`: {e}"))?;
    if !status.success() {
        return Err(format!("`cargo build --bins` for `{pkg}` exited with {status}"));
    }

    let subdir = if profile == "release" { "release" } else { "debug" };
    for bin in bins {
        let bin_path = target_dir.join(subdir).join(bin);
        if !bin_path.exists() {
            return Err(format!(
                "expected `{}` after `cargo build --bins -p {pkg}`, not found",
                bin_path.display()
            ));
        }
        println!("cargo:rustc-env=CARGO_BIN_EXE_{bin}={}", bin_path.display());
    }
    println!("cargo:rerun-if-changed={manifest_dir}");
    Ok(())
}
"#;
/// Top-level member entries that must NOT be symlinked into the bridge dir.
///
/// They either collide with bridge-synthesised files (`Cargo.toml`,
/// `build.rs`), are noise cargo regenerates anyway (`Cargo.lock`), or
/// would be actively harmful (`target/` creates parallel-build recursion,
/// `.git/` confuses tooling that walks up).
const BRIDGE_SKIPLIST: &[&str] = &["Cargo.toml", "Cargo.lock", "build.rs", "target", ".git"];
/// Crate name used for the `rudzio` dependency lookup in member manifests.
const RUDZIO_DEP: &str = "rudzio";

/// Source of a git-based cargo dependency reference.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GitRef {
    /// Branch name.
    Branch(String),
    /// Specific commit revision (sha or short-sha).
    Rev(String),
    /// Tag name.
    Tag(String),
}

/// Resolved location for the `rudzio` dependency that the aggregator should emit.
///
/// Mirrors the three mutually exclusive ways cargo lets a crate reference
/// another: a local path, a git URL, or a registry version requirement.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RudzioLocation {
    /// Git URL plus an optional branch/rev/tag pin.
    Git {
        /// Optional rev/branch/tag pin (`None` means "default branch").
        reference: Option<GitRef>,
        /// Repository URL.
        url: String,
    },
    /// Filesystem path to the crate root.
    Path(PathBuf),
    /// Registry version requirement plus an optional alternative
    /// registry name. `registry = None` means the default registry
    /// (crates.io); `registry = Some("name")` is a named alternative
    /// declared in `[registries.<name>]` in `.cargo/config.toml`.
    Version {
        /// Optional named alternative registry. The aggregator emits
        /// `registry = "<name>"` verbatim so cargo resolves the dep
        /// against the same registry the consumer declared.
        registry: Option<String>,
        /// Cargo-format version requirement string (e.g. `"=0.1.0"`,
        /// `">=1.2, <2"`).
        req: String,
    },
}

/// Manifest section a `DevDepSpec` was parsed from.
///
/// Distinguishes regular `[dependencies]` from test-only
/// `[dev-dependencies]`. Used by `build_bridge_dependencies` to elide
/// cross-bridge cycles introduced by sibling-bridge redirection: a
/// member's test-only dep on a sibling that itself bridges back is
/// safe to drop from the bridge (the aggregator already links every
/// bridged sibling, so test-time uses still resolve), whereas a
/// regular dep must remain visible to the bridge's own lib compile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Section {
    /// `[dev-dependencies]` only — safe to elide as a sibling-bridge
    /// dep on the bridge crate's own `[dependencies]` table.
    DevOnly,
    /// Appears in `[dependencies]` (and possibly also
    /// `[dev-dependencies]`); must remain visible to the bridge's lib
    /// compile because the member's src tree may reference it
    /// outside test/`#[cfg(rudzio_test)]` gates.
    Regular,
}

impl Section {
    /// True when the dep ONLY came from `[dev-dependencies]`.
    #[inline]
    #[must_use]
    pub const fn is_dev_only(self) -> bool {
        matches!(self, Self::DevOnly)
    }

    /// Merge two section markers: any `Regular` occurrence wins so the
    /// final entry can never be silently dropped from the bridge.
    #[inline]
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::DevOnly, Self::DevOnly) => Self::DevOnly,
            (Self::Regular, _) | (_, Self::Regular) => Self::Regular,
        }
    }
}

/// Description of a single `[dev-dependencies]` (or `[dependencies]`) entry
/// pulled from a workspace member's manifest.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DevDepSpec {
    /// Features explicitly requested by the member.
    pub features: Vec<String>,
    /// Git URL when this dep references a git repo.
    pub git: Option<String>,
    /// Optional rev/branch/tag pin associated with `git`.
    pub git_ref: Option<GitRef>,
    /// Crate name (post-rename if `package = ...` was used).
    pub name: String,
    /// `optional = true` in the member's `[dependencies]` entry.
    ///
    /// Mirrored into the bridge so `dep:X` references in the bridge's
    /// `[features]` table resolve (cargo requires the dep to be
    /// optional for `dep:` syntax to be valid).
    pub optional: bool,
    /// Filesystem path when this dep references a local crate.
    pub path: Option<PathBuf>,
    /// Optional named alternative registry (`registry = "<name>"`).
    ///
    /// `None` means the default registry (crates.io). `Some("fthm")`
    /// means the entry declared `registry = "fthm"` in the source
    /// manifest; the aggregator and bridge crates re-emit this key
    /// verbatim so cargo resolves the dep against the same registry
    /// the consumer originally pointed at. Lost-registry symptom: the
    /// generated `target/rudzio-auto-runner/Cargo.toml` strips the
    /// `registry` key, cargo defaults to crates.io, and the build
    /// breaks asymmetrically (plain `cargo test` keeps working).
    pub registry: Option<String>,
    /// The local entry name under which this dep appears, when `package = ...` renames it.
    pub rename: Option<String>,
    /// Which manifest section this dep originated from
    /// (`[dependencies]` vs `[dev-dependencies]`).
    ///
    /// Used by the bridge generator to elide cross-bridge cycles: when
    /// a member declares a sibling-bridge crate as a test-only dep,
    /// re-emitting it inside the bridge's `[dependencies]` would form
    /// a cycle with the sibling's own regular deps once
    /// sibling-bridge redirection is in effect. Dev-only sibling
    /// entries are dropped from the bridge — the aggregator's own
    /// `[dependencies]` already pulls every bridged sibling in, so the
    /// test-time path stays sound. When the same name appears in BOTH
    /// sections, the regular declaration wins (the merge collapses to
    /// `Section::Regular`).
    pub section: Section,
    /// Whether the member opted to keep cargo's `default-features = true`.
    pub uses_default_features: bool,
    /// Cargo-format version requirement string (`""` when the entry has no `version` key).
    pub version_req: String,
    /// Raw spec from the member's Cargo.toml.
    ///
    /// Used when it says `workspace = true` and we need to defer to the
    /// workspace entry.
    pub workspace_inherited: bool,
}

/// Generator-side description of one workspace member.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MemberPlan {
    /// Names of the member's `[[bin]]` targets.
    pub bin_names: Vec<String>,
    /// Dev-dep entries the aggregator must re-emit so the pulled-in test sources compile.
    pub dev_deps: Vec<DevDepSpec>,
    /// Rust edition declared in the member's `[package] edition`.
    ///
    /// The bridge crate uses this so its generated manifest matches the
    /// compilation semantics of the real source tree it re-points at.
    pub edition: String,
    /// The member's own `[features]` table, mirrored verbatim into the bridge's `[features]`.
    ///
    /// So `cfg(feature = "...")` gates in member source resolve against
    /// the same universe of feature names they would under the member's
    /// own `cargo test`.
    pub features: BTreeMap<String, Vec<String>>,
    /// Opt-in from `[package.metadata.rudzio] force_bridge = true`.
    ///
    /// Forces bridge generation for a member whose own `src/**` carries
    /// no rudzio surface (`#[rudzio::test]` / `#[cfg(any(test,
    /// rudzio_test))]`). Used to bridge passthrough consumers of bridged
    /// siblings: without bridging, the consumer's path-dep on a bridged
    /// crate would link the original member alongside the bridge,
    /// producing two distinct rlibs of the same crate and trait-
    /// mismatch errors throughout the aggregator. Also bypasses the
    /// "must depend on rudzio" filter in `build_plan`, so the consumer
    /// no longer has to add a synthetic `rudzio = { ... }` dev-dep.
    pub force_bridge: bool,
    /// `true` iff the member declares a `[lib]` target.
    ///
    /// (Or has the implicit `src/lib.rs`.) Bin-only crates can't be
    /// listed as regular `[dependencies]` entries — they go into
    /// `[workspace.members]` instead.
    pub has_lib: bool,
    /// `true` iff the member has at least one `src/**` file with a rudzio suite or `rudzio_test` gate.
    ///
    /// Drives bridge-crate generation: the bridge exists specifically to
    /// make `[dev-dependencies]` visible to the member's src tree under
    /// `--cfg rudzio_test`, so members whose src has no rudzio surface
    /// don't need one.
    pub has_src_rudzio_suite: bool,
    /// Absolute path to the member's manifest directory.
    pub manifest_dir: PathBuf,
    /// Cargo package name.
    pub package_name: String,
    /// Features listed in `[package.metadata.rudzio] features = [...]`.
    ///
    /// The member's explicit opt-in for features that should be active
    /// under `cargo rudzio test` but aren't in `default`. These get
    /// unioned with the member's `default` to form the bridge's own
    /// `default` feature list.
    pub rudzio_activated_features: Vec<String>,
    /// Absolute path to the member's `src/lib.rs` (when `has_lib`).
    ///
    /// The bridge crate's `[lib] path` points here so cargo compiles the
    /// real source tree instead of the bridge dir.
    pub src_lib_path: Option<PathBuf>,
    /// Absolute paths to the member's integration-test source files.
    ///
    /// Excludes its `tests/main.rs` shim.
    pub test_files: Vec<PathBuf>,
}

/// Everything the generator needs to emit the aggregator.
///
/// Extracted from `cargo metadata` plus the workspace root's Cargo.toml.
#[derive(Debug)]
#[non_exhaustive]
pub struct Plan {
    /// Members of the workspace that actually depend on rudzio AND
    /// contribute at least one integration-test source file.
    pub members: Vec<MemberPlan>,
    /// Resolved `rudzio` dependency to emit in the aggregator's `[dependencies]` table.
    ///
    /// Derived from how the workspace's own members declare rudzio
    /// (path / git / version), with features unioned across every
    /// member's declaration plus `common` + `build`.
    pub rudzio_spec: RudzioSpec,
    /// Cargo's resolved `target_directory` (host's build cache root).
    pub target_directory: Utf8PathBuf,
    /// Path → version overrides keyed by dep name.
    ///
    /// Pulled from the workspace root's `[workspace.dependencies]`
    /// table. Used when a member's dev-dep entry says `workspace = true`.
    pub workspace_deps: BTreeMap<String, WorkspaceDepSpec>,
    /// Absolute path to the workspace root.
    pub workspace_root: Utf8PathBuf,
}

/// Resolved spec for the `rudzio` dependency the aggregator emits.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RudzioSpec {
    /// Where the dep is hosted (path / git / registry version).
    pub features: Vec<String>,
    /// Resolved location.
    pub location: RudzioLocation,
    /// Whether to keep cargo's `default-features = true`.
    pub uses_default_features: bool,
}

/// Per-member resolved view of the rudzio dep used internally by [`collect_rudzio_spec`].
struct ResolvedRudzio {
    /// Cargo features unioned across all of the member's `rudzio` declarations.
    features: Vec<String>,
    /// Git URL when the resolved dep is git-hosted.
    git: Option<String>,
    /// Optional rev/branch/tag pin associated with `git`.
    git_ref: Option<GitRef>,
    /// Source member's package name (used in error messages).
    member: String,
    /// Filesystem path when the resolved dep is local.
    path: Option<PathBuf>,
    /// Optional named alternative registry observed on the rudzio
    /// declaration (or inherited from `[workspace.dependencies.rudzio]`
    /// when the member said `workspace = true`).
    registry: Option<String>,
    /// Whether the member kept cargo's `default-features = true`.
    uses_default_features: bool,
    /// Cargo version requirement when the resolved dep is registry-hosted.
    version_req: Option<String>,
}

/// `[workspace.dependencies]` entry pulled from the workspace root's manifest.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct WorkspaceDepSpec {
    /// Cargo features explicitly enabled at the workspace level.
    pub features: Vec<String>,
    /// Git URL when the workspace dep is git-hosted.
    pub git: Option<String>,
    /// Optional rev/branch/tag pin associated with `git`.
    pub git_ref: Option<GitRef>,
    /// Filesystem path when the workspace dep is local.
    pub path: Option<PathBuf>,
    /// Optional named alternative registry (`registry = "<name>"`).
    ///
    /// `None` means the default registry; `Some("fthm")` propagates to
    /// every member declaring this dep via `workspace = true`, so the
    /// aggregator's regenerated dep table keeps the same registry
    /// attribution the consumer's workspace originally declared.
    pub registry: Option<String>,
    /// Whether to keep cargo's `default-features = true`.
    pub uses_default_features: bool,
    /// Registry version requirement (when present).
    pub version_req: Option<String>,
}

/// `syn::visit_mut::VisitMut` walker for [`rewrite_crate_paths`].
/// Mutates every `crate::<helper>::*` path to `crate::<synth>::*`
/// where `helper` is a key of `helpers`. Non-matching paths pass
/// through untouched.
struct CratePathRewriter<'helpers> {
    /// `helper_name -> synth_name` substitution table.
    helpers: &'helpers BTreeMap<String, String>,
}

impl VisitMut for CratePathRewriter<'_> {
    fn visit_path_mut(&mut self, i: &mut syn::Path) {
        if i.leading_colon.is_none() {
            let mut iter = i.segments.iter_mut();
            if let Some(first) = iter.next()
                && first.ident == "crate"
                && let Some(second) = iter.next()
            {
                let second_ident = second.ident.to_string();
                if let Some(synth) = self.helpers.get(&second_ident) {
                    let span = second.ident.span();
                    second.ident = syn::Ident::new(synth, span);
                }
            }
        }
        visit_mut::visit_path_mut(self, i);
    }

    fn visit_use_tree_mut(&mut self, i: &mut syn::UseTree) {
        if let syn::UseTree::Path(use_path) = i
            && use_path.ident == "crate"
        {
            // The `use crate::*` head — rewrite the first ident of
            // the sub-tree (which is the helper name we care about)
            // when it appears in our substitution table. Glob (`*`)
            // and bare group (`{...}`) variants are handled by
            // recursing into nested entries via the default walker.
            rewrite_first_use_tree_ident(&mut use_path.tree, self.helpers);
        }
        visit_mut::visit_use_tree_mut(self, i);
    }
}

/// Per-test-binary state for the source-rewrite pass. One per
/// `[[test]]` entry across the workspace.
#[derive(Clone, Debug)]
struct TestBinaryProxy {
    /// Stable id derived from `<member_pkg> + <test_root_dir_name>`,
    /// used both as the proxy directory name under
    /// `<out>/src/proxies/<id>/` and as the prefix segment in every
    /// synthesised helper-mod name. Sanitised to a Rust identifier.
    binary_id: String,
    /// `(helper_name, synth_name)` lookup the syn rewriter uses to
    /// detect-and-redirect `crate::<helper>` paths inside source
    /// files belonging to this binary.
    helper_synth_lookup: BTreeMap<String, String>,
    /// Helper `(synthesised_crate_root_name, proxy_helper_path)`
    /// pairs ready for emission by [`build_main_rs`]. The path is
    /// inside this binary's proxy directory, not at the original
    /// location, so all `crate::<helper>` rewrites in sibling files
    /// see the same `mod` instance.
    helpers_at_crate_root: Vec<TestRootHelper>,
    /// Cargo-style aggregator mount path for the proxy mod.rs (the
    /// value [`build_tests_rs`] writes after the `#[path = ...]`
    /// attribute). Absolute path under `<out>/src/proxies/<id>/`.
    proxy_test_root: PathBuf,
    /// Original test-root entry point (`tests/<name>/mod.rs`).
    test_root: PathBuf,
    /// Test-root's parent directory — used as the strip-prefix when
    /// computing relative paths for proxy emission.
    test_root_dir: PathBuf,
}

/// One sibling-module helper discovered under a member's test root.
///
/// Emitted at the aggregator's crate root so `use crate::<module>`
/// inside `#[path]`-included test files resolves the same way it
/// would under stock cargo's per-test-binary crate-root semantics.
#[derive(Clone, Debug)]
struct TestRootHelper {
    /// Module identifier emitted as `mod <module>;` at the aggregator
    /// crate root. Always a sanitised, non-keyword identifier (raw
    /// identifiers are not needed here — siblings of a Rust `mod.rs`
    /// already have to be plain identifiers in source).
    module: String,
    /// Absolute path to the helper's source file
    /// (`<test_root_dir>/<module>.rs` or
    /// `<test_root_dir>/<module>/mod.rs`).
    path: PathBuf,
}

impl DevDepSpec {
    /// Construct a default `DevDepSpec` keyed only by name.
    ///
    /// All other fields are zero-valued (`uses_default_features = true`)
    /// matching cargo's own dev-dep defaults. Use direct field
    /// assignment to populate the rest.
    #[inline]
    #[must_use]
    pub const fn new(name: String) -> Self {
        Self {
            features: Vec::new(),
            git: None,
            git_ref: None,
            name,
            optional: false,
            path: None,
            registry: None,
            rename: None,
            section: Section::Regular,
            uses_default_features: true,
            version_req: String::new(),
            workspace_inherited: false,
        }
    }
}

impl MemberPlan {
    /// Construct a `MemberPlan` with empty / default fields except for the package name and manifest dir.
    ///
    /// The remaining fields are populated via direct assignment by the
    /// generator (and by tests) after construction.
    #[inline]
    #[must_use]
    pub const fn new(package_name: String, manifest_dir: PathBuf) -> Self {
        Self {
            bin_names: Vec::new(),
            dev_deps: Vec::new(),
            edition: String::new(),
            features: BTreeMap::new(),
            force_bridge: false,
            has_lib: false,
            has_src_rudzio_suite: false,
            manifest_dir,
            package_name,
            rudzio_activated_features: Vec::new(),
            src_lib_path: None,
            test_files: Vec::new(),
        }
    }
}

impl Plan {
    /// Per-consumer-workspace aggregator subdirectory name.
    ///
    /// Returns `<AGGREGATOR_NAME>-<hash>` where `<hash>` is a stable
    /// 64-bit FNV-1a digest of the workspace root path. Two
    /// invocations against the SAME consumer workspace get the same
    /// suffix (so cache reuse works); invocations against DIFFERENT
    /// consumer workspaces get different suffixes.
    ///
    /// This is the cornerstone of parallel-invocation safety: when N
    /// concurrent `cargo rudzio test` invocations share one
    /// `CARGO_TARGET_DIR` (e.g. CI matrix, monorepo dev shell with
    /// `CARGO_TARGET_DIR=$HOME/.cache/cargo`), each invocation writes
    /// to its own per-workspace subdir, so they never clobber each
    /// other's `Cargo.toml` / `build.rs` / `members/` state.
    ///
    /// FNV-1a is chosen over `std::collections::hash_map::DefaultHasher`
    /// for stability across rustc releases: cargo's incremental
    /// caching relies on the subdir name being identical from one
    /// invocation to the next, even after the toolchain updates.
    /// FNV-1a is a fixed, specified algorithm; `DefaultHasher` is
    /// explicitly documented as unstable across releases.
    #[inline]
    #[must_use]
    pub fn aggregator_subdir_name(&self) -> String {
        const FNV_OFFSET_64: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME_64: u64 = 0x0000_0100_0000_01b3;
        let mut hash: u64 = FNV_OFFSET_64;
        for byte in self.workspace_root.as_str().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME_64);
        }
        format!("{AGGREGATOR_NAME}-{hash:016x}")
    }

    /// Default output directory for the generated aggregator crate.
    ///
    /// Joins the resolved `target_directory` (the host's effective
    /// `CARGO_TARGET_DIR`) with [`Self::aggregator_subdir_name`]. The
    /// resulting per-consumer-workspace path keeps parallel
    /// invocations against distinct workspaces isolated even when the
    /// user has a single shared `CARGO_TARGET_DIR` set.
    #[inline]
    #[must_use]
    pub fn default_output_dir(&self) -> PathBuf {
        PathBuf::from(self.target_directory.as_std_path()).join(self.aggregator_subdir_name())
    }

    /// Drop every member whose `package_name` matches one of `excluded`.
    ///
    /// Mirrors cargo's `--exclude`: exact name match against the Cargo
    /// package name. Excludes that don't match any current member are
    /// silently ignored (cargo behaves the same way).
    ///
    /// # Errors
    ///
    /// Returns an error when the exclusion would empty the plan — this
    /// usually means the user paired `--exclude` with a too-narrow
    /// `-p` set; surfacing it is friendlier than a silent "0 tests" run.
    #[inline]
    pub fn exclude_packages(&mut self, excluded: &[String]) -> Result<()> {
        if excluded.is_empty() {
            return Ok(());
        }
        let unwanted: BTreeSet<&str> = excluded.iter().map(String::as_str).collect();
        self.members
            .retain(|member| !unwanted.contains(member.package_name.as_str()));
        if self.members.is_empty() {
            let shown = excluded.join(", ");
            bail!(
                "excluded every rudzio crate from the plan via --exclude {shown} \
                 (check the names, or pair with -p to keep something)"
            );
        }
        Ok(())
    }

    /// Construct an empty `Plan` keyed by workspace root, target directory, and rudzio spec.
    #[inline]
    #[must_use]
    pub const fn new(
        workspace_root: Utf8PathBuf,
        target_directory: Utf8PathBuf,
        rudzio_spec: RudzioSpec,
    ) -> Self {
        Self {
            members: Vec::new(),
            rudzio_spec,
            target_directory,
            workspace_deps: BTreeMap::new(),
            workspace_root,
        }
    }

    /// Restrict the plan to members at or under any path in `roots`.
    ///
    /// Drop every member whose manifest dir is not at-or-under any of
    /// `roots` (recursive: a member at `roots[i]/sub/sub` still matches).
    /// Returns `Err` if no member survives — saves the user from a silent
    /// "0 tests" run when their path typo'd.
    ///
    /// Drop every member whose `package_name` doesn't match one of
    /// `packages`. Match is exact string equality against the Cargo
    /// package name (the hyphenated form, as `cargo -p` accepts).
    ///
    /// # Errors
    ///
    /// Returns an error when no remaining member's package name appears
    /// in `packages` — typically because the user typo'd a name or the
    /// requested package isn't in the rudzio dep graph.
    #[inline]
    pub fn restrict_to_packages(&mut self, packages: &[String]) -> Result<()> {
        let wanted: BTreeSet<&str> = packages.iter().map(String::as_str).collect();
        self.members
            .retain(|member| wanted.contains(member.package_name.as_str()));
        if self.members.is_empty() {
            let shown = packages.join(", ");
            bail!(
                "no rudzio crates match package(s): {shown} (check the name, or run \
                 `cargo rudzio test --list` against the workspace to see what's available)"
            );
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error when no remaining member matches any of `roots`.
    #[inline]
    pub fn restrict_to_paths(&mut self, roots: &[PathBuf]) -> Result<()> {
        let abs_roots: Vec<PathBuf> = roots
            .iter()
            .map(|root| canonicalize_or_keep(root))
            .collect();
        self.members.retain(|member| {
            let abs = canonicalize_or_keep(&member.manifest_dir);
            member_under_any_root(&abs, &abs_roots)
        });
        if self.members.is_empty() {
            let shown = roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("no rudzio crates found at-or-under: {shown}");
        }
        Ok(())
    }

    /// Restrict every member's `test_files` to those whose `[[test]]`
    /// binary name appears in `wanted`. Members left with zero
    /// surviving `test_files` are dropped from the plan.
    ///
    /// Matches a `test_file` against `wanted` by computing the
    /// `[[test]] name` cargo would assign: the immediate parent
    /// directory's name for `mod.rs` / `main.rs` / `lib.rs`
    /// entries, otherwise the file stem. Same convention as
    /// `inner_module_ident_for` uses for module-ident derivation.
    ///
    /// # Errors
    ///
    /// Returns an error when `wanted` is non-empty but no test
    /// binary survives — typically a typo in the `--test <name>`
    /// argument.
    #[inline]
    pub fn restrict_to_test_binaries(&mut self, wanted: &[String]) -> Result<()> {
        if wanted.is_empty() {
            return Ok(());
        }
        let wanted_set: BTreeSet<&str> = wanted.iter().map(String::as_str).collect();
        for member in &mut self.members {
            member.test_files.retain(|file| {
                let name = test_binary_name_for(file);
                wanted_set.contains(name.as_str())
            });
        }
        self.members.retain(|member| !member.test_files.is_empty());
        if self.members.is_empty() {
            let shown = wanted.join(", ");
            bail!(
                "no rudzio `[[test]]` binaries match `--test {shown}` (check the name, or run \
                 without `--test` to see what's available)"
            );
        }
        Ok(())
    }
}

impl RudzioSpec {
    /// Construct a `RudzioSpec` from its three required fields.
    #[inline]
    #[must_use]
    pub const fn new(
        location: RudzioLocation,
        features: Vec<String>,
        uses_default_features: bool,
    ) -> Self {
        Self {
            features,
            location,
            uses_default_features,
        }
    }
}

impl WorkspaceDepSpec {
    /// Construct a default `WorkspaceDepSpec`.
    ///
    /// All fields default to empty; `uses_default_features` matches
    /// cargo's own true default.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            features: Vec::new(),
            git: None,
            git_ref: None,
            path: None,
            registry: None,
            uses_default_features: true,
            version_req: None,
        }
    }
}

impl Default for WorkspaceDepSpec {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Pure helper: is `member_dir` at-or-under any path in `roots`?
///
/// Both sides are expected to be already-canonicalized absolute paths so
/// comparison is component-wise. Lifted out of
/// [`Plan::restrict_to_paths`] so it's testable without touching the
/// filesystem.
#[inline]
#[must_use]
pub fn member_under_any_root(member_dir: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| member_dir.starts_with(root))
}

/// Canonicalize `path`, falling back to the input when the OS call fails.
fn canonicalize_or_keep(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reconcile the workspace's `cargo metadata` output into a `Plan` for the generator.
fn build_plan(metadata: &Metadata) -> Result<Plan> {
    let workspace_root_std = metadata.workspace_root.as_std_path();
    let workspace_deps =
        read_workspace_deps(workspace_root_std).context("reading workspace root Cargo.toml")?;

    let mut members: Vec<MemberPlan> = Vec::new();
    let mut runtime_features: BTreeSet<String> = BTreeSet::new();

    let member_ids: BTreeSet<_> = metadata.workspace_members.iter().collect();
    for pkg in &metadata.packages {
        if !member_ids.contains(&pkg.id) {
            continue;
        }
        // Must depend on rudzio somewhere (normal or dev). The opt-in
        // `[package.metadata.rudzio] force_bridge = true` bypasses this
        // gate so passthrough consumers of bridged siblings don't have
        // to add a synthetic rudzio dev-dep just to be eligible.
        let rudzio_deps: Vec<_> = pkg
            .dependencies
            .iter()
            .filter(|dep| dep.name == RUDZIO_DEP)
            .collect();
        let force_bridge =
            load_rudzio_force_bridge(pkg.manifest_path.as_std_path()).with_context(|| {
                format!(
                    "loading `[package.metadata.rudzio].force_bridge` from {}",
                    pkg.manifest_path.as_std_path().display()
                )
            })?;
        if rudzio_deps.is_empty() && !force_bridge {
            continue;
        }
        for rdep in &rudzio_deps {
            for feat in &rdep.features {
                if feat.starts_with("runtime-") {
                    let _newly_inserted: bool = runtime_features.insert(feat.clone());
                }
            }
        }

        if let Some(plan) = build_member_plan(pkg)? {
            members.push(plan);
        }
    }

    // Bug #5: enumerate rudzio-using path-dep members reachable from
    // workspace members, so `cargo rudzio test -p <name>` matches the
    // FULL set of crates the aggregator actually pulls in — not just
    // the direct `[workspace] members = [...]` entries. Transitive
    // path-deps that live in a sibling workspace (e.g. an outer ws
    // dev-depending on an inner ws's crate via `path = "../../inner/x"`)
    // appear in the aggregator's runtime test set but were invisible
    // to `-p` filtering before this pass.
    extend_with_transitive_path_dep_members(&mut members, &mut runtime_features)?;

    members.sort_by(|left, right| left.package_name.cmp(&right.package_name));

    let initial_spec = collect_rudzio_spec(&members, &workspace_deps, workspace_root_std)
        .context("deriving rudzio dependency spec for the aggregator")?;
    let rudzio_spec = inject_required_features(initial_spec, &runtime_features);

    Ok(Plan {
        workspace_root: metadata.workspace_root.clone(),
        target_directory: metadata.target_directory.clone(),
        members,
        rudzio_spec,
        workspace_deps,
    })
}

/// Walk every member's `dev_deps` for path entries. For each
/// path-dep whose manifest declares a rudzio dependency (or
/// `force_bridge`), build a `MemberPlan` and append it to `members`.
/// Recursive: a transitive path-dep's own path-deps get walked too.
/// Cycle-safe via the canonical-path visited set seeded with the
/// existing members' manifest dirs.
///
/// # Errors
///
/// Returns an error when running `cargo metadata --no-deps` on a
/// transitive path-dep's manifest fails (malformed manifest,
/// missing file). Read failures on the manifest path itself are
/// non-fatal: a path-dep pointing at a missing dir is skipped so
/// the outer plan still builds.
fn extend_with_transitive_path_dep_members(
    members: &mut Vec<MemberPlan>,
    runtime_features: &mut BTreeSet<String>,
) -> Result<()> {
    let mut visited: BTreeSet<PathBuf> = members
        .iter()
        .map(|member| canonicalize_or_keep(member.manifest_dir.as_path()))
        .collect();
    let mut frontier: VecDeque<PathBuf> = members
        .iter()
        .flat_map(|member| member.dev_deps.iter())
        .filter_map(|dep| {
            dep.path
                .as_ref()
                .map(|raw_path| canonicalize_or_keep(raw_path))
        })
        .filter(|path| !visited.contains(path))
        .collect();
    while let Some(path_dep_dir) = frontier.pop_front() {
        if !visited.insert(path_dep_dir.clone()) {
            continue;
        }
        let manifest = path_dep_dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let Ok(sub_metadata) = MetadataCommand::new()
            .manifest_path(&manifest)
            .no_deps()
            .exec()
        else {
            continue;
        };
        for pkg in &sub_metadata.packages {
            let pkg_manifest = pkg.manifest_path.as_std_path().to_path_buf();
            let Some(pkg_parent) = pkg_manifest.parent() else {
                continue;
            };
            let canon_pkg_dir = canonicalize_or_keep(pkg_parent);
            // Only process the specific package whose manifest_dir
            // matches the path-dep target. The sub-metadata may
            // include sibling members of an inner workspace; we
            // don't want to silently inherit ALL of them — only
            // the one the outer workspace explicitly path-deps.
            if canon_pkg_dir != path_dep_dir {
                continue;
            }
            let rudzio_deps: Vec<_> = pkg
                .dependencies
                .iter()
                .filter(|dep| dep.name == RUDZIO_DEP)
                .collect();
            let force_bridge = load_rudzio_force_bridge(&pkg_manifest).with_context(|| {
                format!(
                    "loading `[package.metadata.rudzio].force_bridge` from {}",
                    pkg_manifest.display()
                )
            })?;
            if rudzio_deps.is_empty() && !force_bridge {
                continue;
            }
            for rdep in &rudzio_deps {
                for feat in &rdep.features {
                    if feat.starts_with("runtime-") {
                        let _newly_inserted: bool = runtime_features.insert(feat.clone());
                    }
                }
            }
            if let Some(plan) = build_member_plan(pkg)? {
                // Push this transitive member's own path-deps onto
                // the frontier so we walk the full reachable graph.
                for dep in &plan.dev_deps {
                    if let Some(raw_path) = &dep.path {
                        let canon = canonicalize_or_keep(raw_path);
                        if !visited.contains(&canon) {
                            frontier.push_back(canon);
                        }
                    }
                }
                members.push(plan);
            }
        }
    }
    Ok(())
}

/// Construct a `MemberPlan` for one workspace package.
///
/// Reads the member's manifest for excluded test files, dev-deps, and
/// rudzio-activated features, walks its `cargo metadata` targets to pick
/// up bin / lib status, and bundles them all together. Returns
/// `Ok(None)` only if the manifest path has no parent directory (which
/// would indicate a malformed `cargo metadata` payload — defensive
/// rather than load-bearing).
fn build_member_plan(pkg: &Package) -> Result<Option<MemberPlan>> {
    let Some(manifest_parent) = pkg.manifest_path.parent() else {
        return Ok(None);
    };
    let manifest_dir = manifest_parent.to_path_buf();
    let manifest_dir_std = manifest_dir.as_std_path().to_path_buf();

    let exclude_list =
        load_rudzio_exclude_list(pkg.manifest_path.as_std_path()).with_context(|| {
            format!(
                "loading `[package.metadata.rudzio].exclude` from {}",
                pkg.manifest_path.as_std_path().display()
            )
        })?;

    let test_files = discover_test_files(pkg, &manifest_dir_std, &exclude_list)
        .with_context(|| format!("discovering test files for `{}`", pkg.name))?;

    let bin_names: Vec<String> = pkg
        .targets
        .iter()
        .filter(|target| {
            target
                .kind
                .iter()
                .any(|kind| matches!(kind, TargetKind::Bin))
        })
        .map(|target| target.name.clone())
        .collect();

    let has_lib = pkg.targets.iter().any(|target| {
        target
            .kind
            .iter()
            .any(|kind| matches!(kind, TargetKind::Lib))
    });

    let src_lib_path = pkg
        .targets
        .iter()
        .find(|target| {
            target
                .kind
                .iter()
                .any(|kind| matches!(kind, TargetKind::Lib))
        })
        .map(|target| target.src_path.as_std_path().to_path_buf());

    let edition = pkg.edition.to_string();

    let dev_deps = read_dev_deps(pkg.manifest_path.as_std_path()).with_context(|| {
        format!(
            "reading dev-deps from {}",
            pkg.manifest_path.as_std_path().display()
        )
    })?;

    let has_src_rudzio_suite = has_lib
        && src_lib_path
            .as_deref()
            .and_then(|lib| lib.parent())
            .is_some_and(detect_src_rudzio_suite);

    let features: BTreeMap<String, Vec<String>> = pkg
        .features
        .iter()
        .map(|(name, deps)| (name.clone(), deps.clone()))
        .collect();

    let rudzio_activated_features = load_rudzio_activated_features(pkg.manifest_path.as_std_path())
        .with_context(|| {
            format!(
                "loading `[package.metadata.rudzio].features` from {}",
                pkg.manifest_path.as_std_path().display()
            )
        })?;

    let force_bridge =
        load_rudzio_force_bridge(pkg.manifest_path.as_std_path()).with_context(|| {
            format!(
                "loading `[package.metadata.rudzio].force_bridge` from {}",
                pkg.manifest_path.as_std_path().display()
            )
        })?;

    Ok(Some(MemberPlan {
        package_name: pkg.name.to_string(),
        manifest_dir: manifest_dir_std,
        test_files,
        bin_names,
        has_lib,
        dev_deps,
        edition,
        src_lib_path,
        has_src_rudzio_suite,
        force_bridge,
        features,
        rudzio_activated_features,
    }))
}

/// Substring scan of `src_root` for rudzio suite/`rudzio_test` markers.
///
/// Scans every `*.rs` file under `src_root` for the markers that
/// indicate a rudzio suite or `--cfg rudzio_test`-gated module lives
/// there: `rudzio::suite` and `rudzio_test`. False positives (e.g. the
/// strings in a comment) are harmless — they produce a bridge crate for
/// a member that didn't strictly need one. False negatives would mean
/// `cargo rudzio test --via-bridge` silently drops that member's src
/// tests, which is why we keep the substring set deliberately broad.
/// The scan walks sub-directories (`src/foo/bar.rs` counts) but does not
/// descend symlinks and ignores non-UTF-8 files (unreadable → assume no
/// markers, which matches "false negatives are worse than false
/// positives" conservatively in the other direction — if the file is
/// unreadable the aggregator build will surface its own error).
#[inline]
#[must_use]
pub fn detect_src_rudzio_suite(src_root: &Path) -> bool {
    if !src_root.is_dir() {
        return false;
    }
    let mut stack: Vec<PathBuf> = vec![src_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_symlink() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if text.contains("rudzio::suite") || text.contains("rudzio_test") {
                return true;
            }
        }
    }
    false
}

/// Find `#[cfg(test)]`-gated modules in `member`'s src tree that are not broadened for rudzio AND actually contain test functions.
///
/// Walks `member`'s `src/**/*.rs`, parses each file with syn, and
/// flags every `#[cfg(test)] mod X { ... }` whose body actually
/// contains test functions (`#[test]`, `#[rudzio::test]`, or
/// `#[bench]`). Modules without test functions — typical for
/// `#[cfg(test)] mod` blocks that hold helper consts / type aliases /
/// fixtures — stay quiet; warning on those was pure noise (Bug #4:
/// `file-v3` emitted ~80 false-positive warnings per build,
/// `postgres` + `fathom_containers` added more).
///
/// Files that opt-in via `// rudzio_test` marker comment OR contain
/// `rudzio::suite` / `rudzio_test` somewhere stay skipped entirely.
///
/// Syn parse errors fall through silently: unparseable source
/// already breaks compilation diagnostics loud enough that a
/// missing cfg-test warning is not the most useful signal.
#[inline]
#[must_use]
pub fn scan_unbroadened_cfg_test_mods(member: &MemberPlan) -> Vec<String> {
    let Some(lib) = member.src_lib_path.as_deref() else {
        return Vec::new();
    };
    let Some(src_root) = lib.parent() else {
        return Vec::new();
    };
    if !src_root.is_dir() {
        return Vec::new();
    }

    let mut out: Vec<String> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![src_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_symlink() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    for path in &files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        if text.contains("rudzio::suite") || text.contains("rudzio_test") {
            // User has opted in to rudzio in this file (either via a
            // suite attribute or via an explicit `rudzio_test` gate);
            // suppress warnings.
            continue;
        }
        let Ok(parsed) = syn::parse_file(&text) else {
            continue;
        };
        scan_items_for_unbroadened_cfg_test_mods(&parsed.items, path, &mut out);
    }
    out
}

/// Recursive walker: visit every `Item::Mod` in `items`, emit one
/// warning per `#[cfg(test)] mod` block whose body actually carries
/// `#[test]` / `#[rudzio::test]` / `#[bench]` functions, and
/// descend into nested mods so deeper layout (`mod outer { #[cfg(test)]
/// mod inner { ... } }`) gets flagged the same way.
fn scan_items_for_unbroadened_cfg_test_mods(
    items: &[syn::Item],
    path: &Path,
    out: &mut Vec<String>,
) {
    for item in items {
        let syn::Item::Mod(mod_item) = item else {
            continue;
        };
        if let Some((_brace, children)) = mod_item.content.as_ref() {
            if let Some(cfg_attr) = find_unbroadened_cfg_test_attr(&mod_item.attrs)
                && mod_body_has_test_fn(children)
            {
                // Cite the cfg attribute's line, not the `mod` token's
                // line — the attribute is where the user broadens the
                // gate, so it's the actionable pointer.
                let line = cfg_attr.span().start().line;
                out.push(format!(
                    "{file}:{line}: `#[cfg(test)] mod {name}` contains \
                     test functions but isn't broadened for rudzio. \
                     `cargo rudzio test` compiles under `--cfg rudzio_test`, \
                     not `--cfg test`, so this module's tests stay invisible. \
                     To migrate this module to rudzio, do ALL three of:\n\
                     \x20\x20 1. broaden the cfg gate to `#[cfg(any(test, rudzio_test))]`\n\
                     \x20\x20 2. wrap the mod in `#[rudzio::suite([(runtime = ..., suite = ..., test = ...)])]`\n\
                     \x20\x20 3. replace each `#[test]` / `#[tokio::test]` with `#[rudzio::test]`\n\
                     Step 1 alone is NOT sufficient -- it makes the mod visible to \
                     `--cfg rudzio_test` compilation, but the `#[test]` fns inside still \
                     vanish because rudzio's runner only collects `#[rudzio::test]` fns. \
                     Alternatively, add a `// rudzio_test` marker comment to silence this \
                     warning when the tests are intentionally cargo-only.",
                    file = path.display(),
                    name = mod_item.ident,
                ));
            }
            scan_items_for_unbroadened_cfg_test_mods(children, path, out);
        }
    }
}

/// Return the first `#[cfg(test)]` attribute in `attrs` that is NOT
/// broadened to `any(test, rudzio_test)`. Returns `None` if every
/// cfg attribute is already broadened (or if there is no cfg attr at
/// all). Distinguishes the bare-`test` form from the broadened form
/// by walking the attribute meta-tree; any presence of the
/// `rudzio_test` identifier disables the warning even when `test`
/// is also there.
///
/// Returns the attribute (rather than a `bool`) so the caller can
/// cite its source span when emitting the diagnostic — that's the
/// line the user broadens to silence the warning.
///
/// **Polarity tracking:** `not(test)` is NOT a `test`-gated mod —
/// the gate is true exactly when `test` is OFF, so the mod isn't
/// hidden from rudzio's runner by lacking `--cfg test`. The walker
/// flips polarity on entering `not(...)` so `test` inside `not()`
/// doesn't trigger the warning. `any(test, ...)` and `all(test, ...)`
/// preserve polarity.
fn find_unbroadened_cfg_test_attr(attrs: &[syn::Attribute]) -> Option<&syn::Attribute> {
    attrs.iter().find(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let mut bare_test = false;
        let mut has_rudzio_test = false;
        let _walk_top: syn::Result<()> = attr.parse_nested_meta(|meta| {
            walk_cfg_meta(&meta, false, &mut bare_test, &mut has_rudzio_test);
            Ok(())
        });
        bare_test && !has_rudzio_test
    })
}

/// Recursively walk a `cfg(...)` meta tree, setting `bare_test` /
/// `has_rudzio_test` when those idents appear in a POSITIVE context
/// (i.e. not inside a `not(...)`). `inside_not` flips on every
/// `not(...)` recursion. `any(...)` and `all(...)` preserve the
/// surrounding polarity.
fn walk_cfg_meta(
    meta: &ParseNestedMeta<'_>,
    inside_not: bool,
    bare_test: &mut bool,
    has_rudzio_test: &mut bool,
) {
    if meta.path.is_ident("test") {
        if !inside_not {
            *bare_test = true;
        }
        return;
    }
    if meta.path.is_ident("rudzio_test") {
        if !inside_not {
            *has_rudzio_test = true;
        }
        return;
    }
    let entering_not = meta.path.is_ident("not");
    let _walk_inner: syn::Result<()> = meta.parse_nested_meta(|inner| {
        walk_cfg_meta(
            &inner,
            inside_not || entering_not,
            bare_test,
            has_rudzio_test,
        );
        Ok(())
    });
}

/// `true` when any item in `body` is a function annotated with one
/// of the cargo / rudzio test attributes. Drives the Bug #4 narrow:
/// a mod that ONLY contains helper items (consts, type aliases,
/// non-test fns) gets quietly skipped.
fn mod_body_has_test_fn(body: &[syn::Item]) -> bool {
    body.iter().any(|item| {
        let syn::Item::Fn(fn_item) = item else {
            return false;
        };
        fn_item.attrs.iter().any(|attr| {
            let path = attr.path();
            if path.is_ident("test") || path.is_ident("bench") {
                return true;
            }
            // `#[rudzio::test]` — two-segment path.
            let mut iter = path.segments.iter();
            iter.next().is_some_and(|seg| seg.ident == "rudzio")
                && iter.next().is_some_and(|seg| seg.ident == "test")
                && iter.next().is_none()
        })
    })
}

/// Plan-level convenience: scan every member's src tree and return the
/// union of warnings in member order (matching `Plan.members` which
/// `build_plan` sorts by package name, so output is deterministic).
#[inline]
#[must_use]
pub fn scan_unbroadened_cfg_test_mods_in_plan(plan: &Plan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for member in &plan.members {
        out.extend(scan_unbroadened_cfg_test_mods(member));
    }
    out
}

/// Add the features the aggregator unconditionally needs (`common`,
/// `build`) plus any `runtime-*` feature requested by at least one
/// member's metadata-resolved rudzio dep — `runtime-*` features come
/// from `cargo metadata`'s dependency view (which already factors in
/// renames / cfg-target gating), so members may activate runtimes that
/// don't appear in the raw dev-dep features list we collected from the
/// member's Cargo.toml.
fn inject_required_features(
    mut spec: RudzioSpec,
    runtime_features: &BTreeSet<String>,
) -> RudzioSpec {
    let mut feats: BTreeSet<String> = spec.features.into_iter().collect();
    let _common_inserted: bool = feats.insert("common".to_owned());
    let _build_inserted: bool = feats.insert("build".to_owned());
    for feat in runtime_features {
        let _runtime_inserted: bool = feats.insert(feat.clone());
    }
    spec.features = feats.into_iter().collect();
    spec
}

/// Walk every rudzio-using member's raw Cargo.toml and reconcile their
/// `rudzio` declarations into a single `RudzioSpec`.
///
/// Rules:
/// - A member can declare `rudzio` under `[dependencies]`,
///   `[dev-dependencies]`, or the `[target.'cfg(...)'.*]` variants of
///   either. (`read_dev_deps` already collects all four sections — so
///   `MemberPlan::dev_deps` is the right input.)
/// - `workspace = true` resolves via `[workspace.dependencies] rudzio =
///   { ... }` in the workspace root's Cargo.toml.
/// - Across members, all declarations must reference rudzio in a way
///   that can be unified into one location: at most one of `path` /
///   `git` may appear. If both surface, we error with a clear message
///   so the user fixes the inconsistency rather than getting a confused
///   compile error in the generated aggregator.
/// - Path beats version-only when both surface (path is more specific).
///   Git beats version-only the same way.
/// - Features union across every declaration; `default-features`
///   ANDs (default-on stays on only if every declaration agreed).
/// - When NO member declares rudzio in a parseable way (defensive — we
///   already filter to rudzio-using members upstream, so this only
///   triggers for malformed manifests), fall back to a path dep on the
///   workspace root with `default-features = true`. This preserves the
///   in-rudzio-repo dogfood behaviour even if every member's declaration
///   somehow becomes unreadable.
///
/// # Errors
///
/// Returns an error when members reference rudzio inconsistently (mixed
/// path/git locations) or when no declaration carries a usable
/// path/git/version source.
#[inline]
pub fn collect_rudzio_spec(
    members: &[MemberPlan],
    workspace_deps: &BTreeMap<String, WorkspaceDepSpec>,
    workspace_root: &Path,
) -> Result<RudzioSpec> {
    let resolved: Vec<ResolvedRudzio> = resolve_rudzio_declarations(members, workspace_deps)?;

    if resolved.is_empty() {
        // Defensive fallback: something is off (we already filtered to
        // rudzio-using members upstream, so this should not happen),
        // but emit a usable spec so the aggregator at least has a
        // chance to compile inside rudzio's own repo.
        return Ok(RudzioSpec {
            location: RudzioLocation::Path(workspace_root.to_path_buf()),
            features: Vec::new(),
            uses_default_features: true,
        });
    }

    // Inconsistency check: at most one of `path` / `git` across all
    // members. Mixing them produces a spec the aggregator can't honour.
    let path_holder = resolved.iter().find(|entry| entry.path.is_some());
    let git_holder = resolved.iter().find(|entry| entry.git.is_some());
    if let (Some(path_entry), Some(git_entry)) = (path_holder, git_holder) {
        bail!(
            "rudzio is declared inconsistently across workspace members: `{}` uses path={}, `{}` uses git={} — aggregator can't unify these",
            path_entry.member,
            path_entry
                .path
                .as_ref()
                .map(|displayable| displayable.display().to_string())
                .unwrap_or_default(),
            git_entry.member,
            git_entry.git.clone().unwrap_or_default(),
        );
    }

    // Pick location. Path > git > version. (Path beats version because
    // it pins to a specific local checkout; git beats version because
    // it pins to a specific revision.)
    let location = if let Some(path_buf) = path_holder.and_then(|entry| entry.path.clone()) {
        // Canonicalize so `.` / `..` segments inherited from member
        // manifests like `path = "."` don't leak into the aggregator's
        // emitted spec (cosmetic — cargo accepts both, but matching the
        // workspace_root form keeps regenerated output stable).
        let normalized = fs::canonicalize(&path_buf).unwrap_or(path_buf);
        RudzioLocation::Path(normalized)
    } else if let Some(url) = git_holder.and_then(|entry| entry.git.clone()) {
        // For the git ref, take the first non-None ref encountered (git
        // declarations without `rev`/`branch`/`tag` mean "default branch",
        // which we encode as None — cargo treats absence the same way).
        let reference = resolved.iter().find_map(|entry| entry.git_ref.clone());
        RudzioLocation::Git { url, reference }
    } else {
        // No path or git: every declaration is version-only (or
        // workspace-inherited from a version-only entry). Take the
        // first non-None version_req and pair it with the first
        // non-None registry across the same declarations (cargo
        // doesn't permit mixed registries on the same dep name; the
        // first wins for the spec we emit).
        let req = resolved
            .iter()
            .find_map(|entry| entry.version_req.clone())
            .ok_or_else(|| {
                anyhow!(
                    "rudzio is declared by workspace members but none of the declarations carry `path`, `git`, or `version` \u{2014} aggregator can't reference rudzio"
                )
            })?;
        let registry = resolved.iter().find_map(|entry| entry.registry.clone());
        RudzioLocation::Version { req, registry }
    };

    let mut features: BTreeSet<String> = BTreeSet::new();
    let mut uses_default_features = true;
    for resolved_entry in &resolved {
        for feat in &resolved_entry.features {
            let _feature_inserted: bool = features.insert(feat.clone());
        }
        uses_default_features &= resolved_entry.uses_default_features;
    }

    Ok(RudzioSpec {
        location,
        features: features.into_iter().collect(),
        uses_default_features,
    })
}

/// Walk every member's dev-deps and build the resolved rudzio entry list used by `collect_rudzio_spec`.
fn resolve_rudzio_declarations(
    members: &[MemberPlan],
    workspace_deps: &BTreeMap<String, WorkspaceDepSpec>,
) -> Result<Vec<ResolvedRudzio>> {
    let mut resolved: Vec<ResolvedRudzio> = Vec::new();
    for member in members {
        for dep in &member.dev_deps {
            // `dep.name` is the package name post-rename. Rudzio is only
            // ever referenced by its own crate name (no rename), so a
            // straightforward name match is correct.
            if dep.name != RUDZIO_DEP {
                continue;
            }
            if dep.workspace_inherited {
                let ws = workspace_deps.get(RUDZIO_DEP).ok_or_else(|| {
                    anyhow!(
                        "member `{}` declares `rudzio = {{ workspace = true }}` but the workspace root has no `[workspace.dependencies.rudzio]` entry",
                        member.package_name
                    )
                })?;
                let mut feats: Vec<String> = ws.features.clone();
                feats.extend(dep.features.iter().cloned());
                // Member-level `registry = "<name>"` wins over the
                // workspace entry's (member overrides workspace, same
                // as cargo's own merge rules for features /
                // default-features). Either side may be `None`.
                let registry = dep.registry.clone().or_else(|| ws.registry.clone());
                resolved.push(ResolvedRudzio {
                    member: member.package_name.clone(),
                    path: ws.path.clone(),
                    git: ws.git.clone(),
                    git_ref: ws.git_ref.clone(),
                    registry,
                    version_req: ws.version_req.clone(),
                    features: feats,
                    uses_default_features: dep.uses_default_features && ws.uses_default_features,
                });
            } else {
                let version_req = if dep.version_req.is_empty() {
                    None
                } else {
                    Some(dep.version_req.clone())
                };
                resolved.push(ResolvedRudzio {
                    member: member.package_name.clone(),
                    path: dep.path.clone(),
                    git: dep.git.clone(),
                    git_ref: dep.git_ref.clone(),
                    registry: dep.registry.clone(),
                    version_req,
                    features: dep.features.clone(),
                    uses_default_features: dep.uses_default_features,
                });
            }
        }
    }
    Ok(resolved)
}

/// Pick the right inline-table shape for the aggregator's rudzio dep:
/// route through rudzio's bridge when rudzio is itself a bridged
/// workspace member, otherwise fall back to the workspace-derived spec.
#[inline]
#[must_use]
fn aggregator_rudzio_inline_table(plan: &Plan) -> InlineTable {
    plan.members
        .iter()
        .find(|member| member.package_name == RUDZIO_DEP && bridge_applies_to(member))
        .map_or_else(
            || build_rudzio_inline_table(&plan.rudzio_spec),
            |member| build_rudzio_bridge_inline_table(member, &plan.rudzio_spec),
        )
}

/// Render the aggregator's rudzio dep as a sibling-bridge entry pointing
/// at `rudzio_member`'s generated bridge crate.
///
/// Used when rudzio is itself a workspace member of the consumer's
/// workspace (the rudzio repo dogfooding itself) — without this, the
/// aggregator's `[dependencies] rudzio = { path = ... }` would compile a
/// second rudzio rlib alongside the bridges' shared one, and at startup
/// the linkme `#[distributed_slice]` machinery rudzio uses for test
/// discovery panics with "duplicate `#[distributed_slice]`". Features
/// and `default-features` carry over from `spec` so the union of every
/// member's requested rudzio runtimes still reaches the bridge.
#[inline]
#[must_use]
pub fn build_rudzio_bridge_inline_table(
    rudzio_member: &MemberPlan,
    spec: &RudzioSpec,
) -> InlineTable {
    let mut tbl = InlineTable::new();
    tbl.insert(
        "path",
        Value::String(Formatted::new(format!(
            "./members/{}",
            bridge_dir_name(rudzio_member)
        ))),
    );
    tbl.insert(
        "package",
        Value::String(Formatted::new(bridge_package_name(rudzio_member))),
    );
    if !spec.features.is_empty() {
        let mut feats = Array::new();
        for feat in &spec.features {
            feats.push(Value::String(Formatted::new(feat.clone())));
        }
        tbl.insert("features", Value::Array(feats));
    }
    if !spec.uses_default_features {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }
    tbl
}

/// Render a `RudzioSpec` as a Cargo dependency inline table.
///
/// Emits the minimal correct shape: location keys (`path` / `git` + ref
/// / `version`) followed by `features` (only when non-empty) and
/// `default-features = false` (only when the user opted out).
#[inline]
#[must_use]
pub fn build_rudzio_inline_table(spec: &RudzioSpec) -> InlineTable {
    let mut tbl = InlineTable::new();
    match &spec.location {
        RudzioLocation::Path(path_buf) => {
            tbl.insert(
                "path",
                Value::String(Formatted::new(path_buf.to_string_lossy().into_owned())),
            );
        }
        RudzioLocation::Git { url, reference } => {
            tbl.insert("git", Value::String(Formatted::new(url.clone())));
            match reference {
                Some(GitRef::Rev(rev)) => {
                    tbl.insert("rev", Value::String(Formatted::new(rev.clone())));
                }
                Some(GitRef::Branch(branch)) => {
                    tbl.insert("branch", Value::String(Formatted::new(branch.clone())));
                }
                Some(GitRef::Tag(tag)) => {
                    tbl.insert("tag", Value::String(Formatted::new(tag.clone())));
                }
                None => {}
            }
        }
        RudzioLocation::Version { req, registry } => {
            tbl.insert("version", Value::String(Formatted::new(req.clone())));
            if let Some(name) = registry {
                tbl.insert("registry", Value::String(Formatted::new(name.clone())));
            }
        }
    }
    if !spec.features.is_empty() {
        let mut feats = Array::new();
        for feat in &spec.features {
            feats.push(Value::String(Formatted::new(feat.clone())));
        }
        tbl.insert("features", Value::Array(feats));
    }
    if !spec.uses_default_features {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }
    tbl
}

/// For each member, list the integration-test source files that should
/// be `#[path]`-included in the aggregator.
///
/// Algorithm:
/// - If the manifest declares one or more explicit `[[test]]` entries,
///   use the `path` fields of each, minus any that resolve to the
///   `tests/main.rs` shim (which contains its own `#[rudzio::main]`).
/// - Otherwise (no explicit entries, or `autotests = false` with no
///   entries) fall back to scanning immediate children of `tests/*.rs`.
///
/// Files whose absolute path matches any entry in `exclude` (sourced from
/// `[package.metadata.rudzio].exclude` in the member's Cargo.toml) are
/// filtered out — useful for trybuild-driven tests whose manifest-dir-
/// relative paths would break when `#[path]`-included into the aggregator.
fn discover_test_files(
    pkg: &Package,
    manifest_dir: &Path,
    exclude: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let tests_dir = manifest_dir.join("tests");
    let main_shim = tests_dir.join("main.rs");

    // Explicit `[[test]]` entries, minus the aggregator shim (`tests/main.rs`
    // — the file containing its own `#[rudzio::main]`). Rudzio crates set
    // `autotests = false` and declare a single `[[test]] path = "tests/main.rs"`,
    // so the explicit list collapses to empty here and we fall through to
    // directory scanning.
    let explicit: Vec<PathBuf> = pkg
        .targets
        .iter()
        .filter(|target| {
            target
                .kind
                .iter()
                .any(|kind| matches!(kind, TargetKind::Test))
        })
        .map(|target| target.src_path.as_std_path().to_path_buf())
        .filter(|path_buf| path_buf != &main_shim)
        .collect();

    let mut files: Vec<PathBuf> = if explicit.is_empty() {
        if !tests_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for raw_entry in
            fs::read_dir(&tests_dir).with_context(|| format!("reading {}", tests_dir.display()))?
        {
            let entry = raw_entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            if path == main_shim {
                continue;
            }
            out.push(path);
        }
        out
    } else {
        explicit
    };

    if !exclude.is_empty() {
        files.retain(|file| !exclude.iter().any(|excluded| paths_equal(file, excluded)));
    }

    files.sort();
    files.dedup();
    Ok(files)
}

/// Load `[package.metadata.rudzio].exclude` from the member's Cargo.toml.
///
/// Returns absolute paths (each entry joined against the manifest's parent
/// directory). An absent section yields an empty vec; a present but non-
/// array-of-strings value is a hard error so misconfigurations are caught
/// at aggregator-generation time rather than silently ignored.
fn load_rudzio_exclude_list(manifest_path: &Path) -> Result<Vec<PathBuf>> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent"))?;

    let Some(package) = doc.get("package").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(metadata) = package.get("metadata").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(rudzio_meta) = metadata.get("rudzio").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(exclude_item) = rudzio_meta.get("exclude") else {
        return Ok(Vec::new());
    };
    let array = exclude_item.as_array().ok_or_else(|| {
        anyhow!(
            "`[package.metadata.rudzio].exclude` in {} must be an array of strings",
            manifest_path.display()
        )
    })?;

    let mut out: Vec<PathBuf> = Vec::with_capacity(array.len());
    for entry in array {
        let path_str = entry.as_str().ok_or_else(|| {
            anyhow!(
                "`[package.metadata.rudzio].exclude` in {} must contain only strings",
                manifest_path.display()
            )
        })?;
        out.push(manifest_dir.join(path_str));
    }
    Ok(out)
}

/// Read the `[package.metadata.rudzio] features = [...]` array from a member manifest.
///
/// These names are features the member opts in to under `cargo rudzio
/// test` — they become part of the bridge's own `default` feature list
/// so cargo activates them when compiling the bridge. Missing → empty
/// vec (no opt-in). Non-array value → hard error so misconfigurations
/// surface at aggregator-generation time.
///
/// # Errors
///
/// Returns an error if the manifest cannot be read/parsed, or if the
/// `[package.metadata.rudzio].features` value is present but not an
/// array of strings.
#[inline]
pub fn load_rudzio_activated_features(manifest_path: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let Some(package) = doc.get("package").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(metadata) = package.get("metadata").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(rudzio_meta) = metadata.get("rudzio").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let Some(features_item) = rudzio_meta.get("features") else {
        return Ok(Vec::new());
    };
    let array = features_item.as_array().ok_or_else(|| {
        anyhow!(
            "`[package.metadata.rudzio].features` in {} must be an array of strings",
            manifest_path.display()
        )
    })?;

    let mut out: Vec<String> = Vec::with_capacity(array.len());
    for entry in array {
        let feature_str = entry.as_str().ok_or_else(|| {
            anyhow!(
                "`[package.metadata.rudzio].features` in {} must contain only strings",
                manifest_path.display()
            )
        })?;
        out.push(feature_str.to_owned());
    }
    Ok(out)
}

/// Load `[package.metadata.rudzio].force_bridge` from the member's Cargo.toml.
///
/// Members that opt in with `force_bridge = true` get bridged even when
/// their own `src/**` carries no rudzio surface. Use this for workspace
/// crates that path-dep on bridged siblings but expose no
/// `#[rudzio::test]` of their own — without bridging, the consumer's
/// path-dep would link the original sibling alongside the bridge,
/// producing two distinct rlibs of the same crate and trait-mismatch
/// errors throughout the aggregator. Setting this also bypasses
/// `build_plan`'s "must depend on rudzio" filter, so the consumer no
/// longer needs a synthetic `rudzio = { ... }` dev-dep just to be
/// considered.
///
/// An absent section yields `false`; a present non-bool value is a hard
/// error so misconfigurations surface at aggregator-generation time.
///
/// # Errors
///
/// Returns an error if the manifest cannot be read/parsed, or if the
/// `[package.metadata.rudzio].force_bridge` value is present but not a
/// bool.
#[inline]
pub fn load_rudzio_force_bridge(manifest_path: &Path) -> Result<bool> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let Some(package) = doc.get("package").and_then(Item::as_table) else {
        return Ok(false);
    };
    let Some(metadata) = package.get("metadata").and_then(Item::as_table) else {
        return Ok(false);
    };
    let Some(rudzio_meta) = metadata.get("rudzio").and_then(Item::as_table) else {
        return Ok(false);
    };
    let Some(force_item) = rudzio_meta.get("force_bridge") else {
        return Ok(false);
    };
    force_item
        .as_value()
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            anyhow!(
                "`[package.metadata.rudzio].force_bridge` in {} must be a bool",
                manifest_path.display()
            )
        })
}

/// Parse the workspace root's `[workspace.dependencies]` table into a map of `WorkspaceDepSpec`.
fn read_workspace_deps(workspace_root: &Path) -> Result<BTreeMap<String, WorkspaceDepSpec>> {
    let manifest_path = workspace_root.join("Cargo.toml");
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    let mut out: BTreeMap<String, WorkspaceDepSpec> = BTreeMap::new();
    let Some(ws) = doc.get("workspace").and_then(Item::as_table) else {
        return Ok(out);
    };
    let Some(deps) = ws.get("dependencies").and_then(Item::as_table) else {
        return Ok(out);
    };
    for (name, item) in deps {
        let spec = extract_workspace_dep_spec(item, workspace_root, name).with_context(|| {
            format!(
                "parsing [workspace.dependencies.{name}] in {}",
                manifest_path.display()
            )
        })?;
        drop(out.insert(name.to_owned(), spec));
    }
    Ok(out)
}

/// Extract a `WorkspaceDepSpec` from a single `[workspace.dependencies]` entry value.
///
/// # Errors
///
/// Returns an error when the entry contains an unrecognised key or a
/// known key with the wrong TOML type. See [`apply_ws_dep_field`] for
/// the rationale (strict-parse is cheap insurance against the same
/// silent-drop bug class that previously let `registry = "<name>"`
/// vanish from generated manifests).
fn extract_workspace_dep_spec(
    item: &Item,
    workspace_root: &Path,
    dep_name: &str,
) -> Result<WorkspaceDepSpec> {
    let mut spec = WorkspaceDepSpec {
        version_req: None,
        path: None,
        git: None,
        git_ref: None,
        features: Vec::new(),
        uses_default_features: true,
        registry: None,
    };
    match item {
        Item::Value(Value::String(version_str)) => {
            spec.version_req = Some(version_str.value().to_owned());
        }
        Item::Value(Value::InlineTable(inline)) => {
            for (key, val) in inline {
                apply_ws_dep_field(&mut spec, key, val, workspace_root, dep_name)?;
            }
        }
        Item::Table(table) => {
            for (key, item_val) in table {
                if let Some(val) = item_val.as_value() {
                    apply_ws_dep_field(&mut spec, key, val, workspace_root, dep_name)?;
                } else {
                    bail!(
                        "workspace dep `{dep_name}` has key `{key}` with a non-value shape (nested table or array-of-tables) — only inline values are supported in `[workspace.dependencies]` entries",
                    );
                }
            }
        }
        Item::None | Item::ArrayOfTables(_) => {
            bail!(
                "workspace dep `{dep_name}` has an unsupported entry shape — expected a string version requirement or an inline/explicit table",
            );
        }
        Item::Value(other) => {
            bail!(
                "workspace dep `{dep_name}` has top-level value of type `{}` — expected a string version requirement or an inline table",
                toml_type_name(other)
            );
        }
    }
    Ok(spec)
}

/// Apply one `key = val` pair from a `[workspace.dependencies]` entry into `spec`.
///
/// # Errors
///
/// Returns an error when `key` is not one of the cargo-recognised
/// workspace-dep keys this parser understands, or when the value's
/// TOML type doesn't match (e.g. `version = 42`). Same rationale as
/// [`apply_dev_dep_field`] — silent drops at this layer mean
/// consumer-side attribution (`registry`, `default-features`, etc.)
/// vanishes without trace; fail loudly so the parser is kept in sync
/// with cargo's actual key set.
fn apply_ws_dep_field(
    spec: &mut WorkspaceDepSpec,
    key: &str,
    val: &Value,
    base: &Path,
    dep_name: &str,
) -> Result<()> {
    match key {
        "version" => {
            spec.version_req = Some(require_string(val, dep_name, "version")?.to_owned());
        }
        "path" => {
            spec.path = Some(base.join(require_string(val, dep_name, "path")?));
        }
        "git" => {
            spec.git = Some(require_string(val, dep_name, "git")?.to_owned());
        }
        "rev" => {
            spec.git_ref = Some(GitRef::Rev(
                require_string(val, dep_name, "rev")?.to_owned(),
            ));
        }
        "branch" => {
            spec.git_ref = Some(GitRef::Branch(
                require_string(val, dep_name, "branch")?.to_owned(),
            ));
        }
        "tag" => {
            spec.git_ref = Some(GitRef::Tag(
                require_string(val, dep_name, "tag")?.to_owned(),
            ));
        }
        "features" => {
            spec.features = require_string_array(val, dep_name, "features")?;
        }
        "default-features" => {
            spec.uses_default_features = require_bool(val, dep_name, "default-features")?;
        }
        "registry" => {
            spec.registry = Some(require_string(val, dep_name, "registry")?.to_owned());
        }
        other => {
            bail!(
                "workspace dep `{dep_name}` carries unrecognised key `{other}`. Either it's a typo, or this version of cargo-rudzio doesn't yet know about it. Recognised keys: version, path, git, rev, branch, tag, features, default-features, registry. Open an issue if you believe `{other}` is a valid cargo workspace-dep key.",
            );
        }
    }
    Ok(())
}

/// Parse both `[dependencies]` and `[dev-dependencies]` (plus their
/// `[target.'cfg(...)'.*]` variants) out of the member's Cargo.toml
/// verbatim. Normal deps are included because `#[path]`-inclusion of a
/// member's integration-test source pulls in references to the member's
/// regular deps (e.g. `macro-internals/tests/transform.rs` says
/// `use syn::...`, which only resolves if `syn` is a dep of the crate
/// actually compiling the source — here, the aggregator). Dev-deps are
/// included for the same reason (`trybuild`, `libc` in fixtures). The
/// `workspace = true` flag is preserved so the aggregator defers to the
/// workspace root's pinned version.
fn read_dev_deps(manifest_path: &Path) -> Result<Vec<DevDepSpec>> {
    let sections: [(&str, Section); 2] = [
        ("dependencies", Section::Regular),
        ("dev-dependencies", Section::DevOnly),
    ];
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent"))?;

    let mut out: Vec<DevDepSpec> = Vec::new();
    for (section_name, section) in sections {
        collect_dev_deps(&doc, &[section_name], manifest_dir, section, &mut out)
            .with_context(|| format!("parsing [{section_name}] in {}", manifest_path.display()))?;
    }

    if let Some(target_tbl) = doc.get("target").and_then(Item::as_table) {
        for (cfg, cfg_item) in target_tbl {
            let Some(cfg_tbl) = cfg_item.as_table() else {
                continue;
            };
            for (section_name, section) in sections {
                if let Some(Item::Table(deps_tbl)) = cfg_tbl.get(section_name) {
                    for (name, item) in deps_tbl {
                        let spec = parse_dev_dep_entry(name, item, manifest_dir, section)
                            .with_context(|| {
                                format!(
                                    "parsing [target.{cfg}.{section_name}.{name}] in {}",
                                    manifest_path.display()
                                )
                            })?;
                        out.push(spec);
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Walk `doc` to the table at `path` and append every entry as a `DevDepSpec` into `out`.
///
/// # Errors
///
/// Returns an error for the first dep entry that fails to parse.
/// Missing tables along `path` are NOT errors — they just mean the
/// section doesn't exist in this manifest, which is normal.
fn collect_dev_deps(
    doc: &DocumentMut,
    path: &[&str],
    manifest_dir: &Path,
    section: Section,
    out: &mut Vec<DevDepSpec>,
) -> Result<()> {
    let mut cur: &Item = doc.as_item();
    for key in path {
        let Some(tbl) = cur.as_table() else {
            return Ok(());
        };
        let Some(next) = tbl.get(key) else {
            return Ok(());
        };
        cur = next;
    }
    let Some(deps_tbl) = cur.as_table() else {
        return Ok(());
    };
    for (name, item) in deps_tbl {
        let spec = parse_dev_dep_entry(name, item, manifest_dir, section)
            .with_context(|| format!("parsing dep entry `{name}`"))?;
        out.push(spec);
    }
    Ok(())
}

/// Parse a single dev/normal dep entry into a `DevDepSpec`.
///
/// # Errors
///
/// Returns an error when the entry's TOML shape isn't a string
/// version requirement, an inline table, or an explicit table — or
/// when one of those tables contains an unrecognised key or a known
/// key with the wrong type. Strict-parse on purpose: every silent
/// drop in this layer caused a downstream debugging session
/// previously (see Bug #6: `registry = "fthm"` vanished without
/// trace), so we fail loudly and force the parser to track cargo's
/// real key set.
fn parse_dev_dep_entry(
    name: &str,
    item: &Item,
    manifest_dir: &Path,
    section: Section,
) -> Result<DevDepSpec> {
    let mut spec = DevDepSpec {
        section,
        name: name.to_owned(),
        rename: None,
        version_req: String::new(),
        path: None,
        registry: None,
        git: None,
        git_ref: None,
        features: Vec::new(),
        uses_default_features: true,
        workspace_inherited: false,
        optional: false,
    };
    match item {
        Item::Value(Value::String(version_str)) => {
            version_str.value().clone_into(&mut spec.version_req);
        }
        Item::Value(Value::InlineTable(inline)) => {
            fill_dev_dep_from_pairs(&mut spec, inline.iter(), manifest_dir)?;
        }
        Item::Table(table) => {
            fill_dev_dep_from_table(&mut spec, table, manifest_dir)?;
        }
        Item::None | Item::ArrayOfTables(_) => {
            bail!(
                "dep `{name}` has an unsupported entry shape — expected a string version requirement or an inline/explicit table",
            );
        }
        Item::Value(other) => {
            bail!(
                "dep `{name}` has top-level value of type `{}` — expected a string version requirement or an inline table",
                toml_type_name(other)
            );
        }
    }
    Ok(spec)
}

/// Apply every `(key, value)` pair from an iterator into `spec`.
///
/// # Errors
///
/// Propagates the first error returned by [`apply_dev_dep_field`].
fn fill_dev_dep_from_pairs<'pair>(
    spec: &mut DevDepSpec,
    entries: impl Iterator<Item = (&'pair str, &'pair Value)>,
    manifest_dir: &Path,
) -> Result<()> {
    for (key, val) in entries {
        apply_dev_dep_field(spec, key, val, manifest_dir)?;
    }
    Ok(())
}

/// Apply every value-typed cell of a TOML `Table` into `spec`.
///
/// # Errors
///
/// Propagates the first error returned by [`apply_dev_dep_field`]. A
/// nested table or array-of-tables under a dep entry is itself an
/// error — cargo dep-table fields are strictly inline values.
fn fill_dev_dep_from_table(
    spec: &mut DevDepSpec,
    table: &Table,
    manifest_dir: &Path,
) -> Result<()> {
    for (key, item) in table {
        let Some(val) = item.as_value() else {
            bail!(
                "dep `{}` has key `{key}` with a non-value shape (nested table or array-of-tables) — only inline values are supported",
                spec.name,
            );
        };
        apply_dev_dep_field(spec, key, val, manifest_dir)?;
    }
    Ok(())
}

/// Apply one `key = val` pair from a dev/normal dep entry into `spec`.
///
/// # Errors
///
/// Returns an error when `key` is not one of the cargo-recognised
/// dep-table keys this parser understands, or when the value's TOML
/// type doesn't match the key's expected shape (e.g. `version = 42`
/// instead of `version = "0.1"`). Silent drops let consumer-side
/// configuration (e.g. `registry = "fthm"`) vanish without diagnostic
/// — every silent drop costs a downstream debugging session, so we
/// fail loudly here and force the parser to be updated whenever cargo
/// grows new keys.
fn apply_dev_dep_field(
    spec: &mut DevDepSpec,
    key: &str,
    val: &Value,
    manifest_dir: &Path,
) -> Result<()> {
    match key {
        "workspace" => {
            let flag = val.as_bool().ok_or_else(|| {
                anyhow!(
                    "dep `{}`: `workspace` must be a boolean, got {}",
                    spec.name,
                    toml_type_name(val)
                )
            })?;
            if flag {
                spec.workspace_inherited = true;
            }
        }
        "version" => {
            spec.version_req = require_string(val, &spec.name, "version")?.to_owned();
        }
        "path" => {
            let text = require_string(val, &spec.name, "path")?;
            spec.path = Some(manifest_dir.join(text));
        }
        "git" => {
            spec.git = Some(require_string(val, &spec.name, "git")?.to_owned());
        }
        "rev" => {
            spec.git_ref = Some(GitRef::Rev(
                require_string(val, &spec.name, "rev")?.to_owned(),
            ));
        }
        "branch" => {
            spec.git_ref = Some(GitRef::Branch(
                require_string(val, &spec.name, "branch")?.to_owned(),
            ));
        }
        "tag" => {
            spec.git_ref = Some(GitRef::Tag(
                require_string(val, &spec.name, "tag")?.to_owned(),
            ));
        }
        "features" => {
            spec.features = require_string_array(val, &spec.name, "features")?;
        }
        "default-features" => {
            spec.uses_default_features = require_bool(val, &spec.name, "default-features")?;
        }
        "optional" => {
            spec.optional = require_bool(val, &spec.name, "optional")?;
        }
        "package" => {
            let text = require_string(val, &spec.name, "package")?;
            spec.rename = Some(spec.name.clone());
            text.clone_into(&mut spec.name);
        }
        "registry" => {
            spec.registry = Some(require_string(val, &spec.name, "registry")?.to_owned());
        }
        other => {
            bail!(
                "dep `{}` carries unrecognised key `{other}`. Either it's a typo, or this version of cargo-rudzio doesn't yet know about it. Recognised keys: workspace, version, path, git, rev, branch, tag, features, default-features, optional, package, registry. Open an issue if you believe `{other}` is a valid cargo key.",
                spec.name,
            );
        }
    }
    Ok(())
}

/// Coerce a `Value` to `&str` or bail with a typed diagnostic.
fn require_string<'val>(val: &'val Value, dep: &str, key: &str) -> Result<&'val str> {
    val.as_str().ok_or_else(|| {
        anyhow!(
            "dep `{dep}`: `{key}` must be a string, got {}",
            toml_type_name(val)
        )
    })
}

/// Coerce a `Value` to `bool` or bail with a typed diagnostic.
fn require_bool(val: &Value, dep: &str, key: &str) -> Result<bool> {
    val.as_bool().ok_or_else(|| {
        anyhow!(
            "dep `{dep}`: `{key}` must be a boolean, got {}",
            toml_type_name(val)
        )
    })
}

/// Coerce a `Value` to a `Vec<String>` (array of strings) or bail.
fn require_string_array(val: &Value, dep: &str, key: &str) -> Result<Vec<String>> {
    let arr = val.as_array().ok_or_else(|| {
        anyhow!(
            "dep `{dep}`: `{key}` must be an array of strings, got {}",
            toml_type_name(val)
        )
    })?;
    let mut out: Vec<String> = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let text = entry.as_str().ok_or_else(|| {
            anyhow!(
                "dep `{dep}`: `{key}[{idx}]` must be a string, got {}",
                toml_type_name(entry)
            )
        })?;
        out.push(text.to_owned());
    }
    Ok(out)
}

/// Render a `Value`'s TOML type as a human-readable string for use
/// in parse-error diagnostics.
const fn toml_type_name(val: &Value) -> &'static str {
    match val {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Float(_) => "float",
        Value::Boolean(_) => "boolean",
        Value::Datetime(_) => "datetime",
        Value::Array(_) => "array",
        Value::InlineTable(_) => "inline table",
    }
}

/// Write the generated aggregator crate to `out_dir`.
///
/// Recreates `Cargo.toml`, `build.rs`, `src/main.rs`, `src/tests.rs`,
/// and the `members/` bridge crates each call. Existing `target/` and
/// `Cargo.lock` artefacts are preserved.
///
/// # Errors
///
/// Returns an error when any filesystem write fails or when the
/// generated bridge / Cargo.toml content cannot be produced.
#[inline]
pub fn write_runner(plan: &Plan, out_dir: &Path) -> Result<()> {
    // Regenerate our own files from scratch. Preserve the aggregator's
    // `target/` directory across invocations — blowing it away on every
    // run would force a full recompile of the entire workspace every
    // time, and on macOS `remove_dir_all` additionally races with cargo
    // lock files still being written (ENOTEMPTY under concurrent IO).
    //
    // ENOENT tolerance: two `cargo rudzio test` invocations against
    // the SAME workspace (same `aggregator_subdir_name`) can race on
    // the cleanup phase below — invocation A removes a file in the
    // ~5 ms window between B's `path.exists()` check (sees: yes) and
    // B's `fs::remove_file` call (sees: ENOENT). The original code
    // surfaced that race as a hard error
    // (`cargo-rudzio: removing .../Cargo.toml: No such file or
    // directory`); treating `NotFound` as success makes the cleanup
    // idempotent without introducing a lock. Other IO errors still
    // propagate.
    let src_dir = out_dir.join("src");
    for path in [
        out_dir.join("Cargo.toml"),
        out_dir.join("Cargo.lock"),
        out_dir.join("build.rs"),
    ] {
        remove_file_tolerating_missing(&path)
            .with_context(|| format!("removing {}", path.display()))?;
    }
    remove_dir_all_tolerating_missing(&src_dir)
        .with_context(|| format!("removing {}", src_dir.display()))?;
    let members_dir = out_dir.join("members");
    remove_dir_all_tolerating_missing(&members_dir)
        .with_context(|| format!("removing {}", members_dir.display()))?;
    fs::create_dir_all(&src_dir).with_context(|| format!("creating {}", src_dir.display()))?;

    for member in &plan.members {
        if !bridge_applies_to(member) {
            continue;
        }
        write_bridge_crate(plan, member, &members_dir).with_context(|| {
            format!("writing bridge crate for member `{}`", member.package_name)
        })?;
    }

    // Bug #10 collision detection: two `[[test]]` binaries declaring
    // `mod <name>;` against DIFFERENT source files must fail loudly
    // here, not silently first-wins or transparently scope. The
    // error chain names every offending binary so the user can
    // rename or alias.
    detect_helper_collisions(plan)?;

    // Materialise the per-test-binary proxy tree under `src/proxies/`
    // BEFORE writing main.rs / tests.rs — both reference the proxy
    // file paths via `#[path = ...]` and cargo will fail at compile
    // time if the files don't yet exist.
    let proxies = collect_test_binary_proxies(plan, out_dir);
    write_test_binary_proxies(out_dir, &proxies)
        .with_context(|| format!("writing test-binary proxies under {}", out_dir.display()))?;

    let cargo_toml = build_cargo_toml(plan)?;
    fs::write(out_dir.join("Cargo.toml"), cargo_toml)
        .with_context(|| format!("writing Cargo.toml under {}", out_dir.display()))?;

    fs::write(src_dir.join("main.rs"), build_main_rs(plan, out_dir))
        .with_context(|| format!("writing src/main.rs under {}", out_dir.display()))?;

    fs::write(src_dir.join("tests.rs"), build_tests_rs(plan, out_dir))
        .with_context(|| format!("writing src/tests.rs under {}", out_dir.display()))?;

    fs::write(out_dir.join("build.rs"), build_build_rs(plan))
        .with_context(|| format!("writing build.rs under {}", out_dir.display()))?;

    Ok(())
}

/// Gates a member must pass to get a bridge crate generated.
///
/// It has to be a lib (bridges re-point `[lib] path`, nothing to
/// re-point for a bin-only crate) AND either have rudzio surface in
/// `src/**` (the usual case — gives the bridge a concrete reason to
/// exist) OR opt in via `[package.metadata.rudzio] force_bridge = true`
/// (the consumer-of-a-bridged-sibling escape hatch).
#[inline]
#[must_use]
pub const fn bridge_applies_to(member: &MemberPlan) -> bool {
    member.has_lib
        && member.src_lib_path.is_some()
        && (member.has_src_rudzio_suite || member.force_bridge)
}

/// Emit `<out>/<normalized>/Cargo.toml` for a member's bridge crate.
///
/// The bridge:
/// - declares `[package] name = "<member>_rudzio_bridge"` so cargo sees
///   it as a distinct crate (avoids collisions with the real member
///   that the IDE / `cargo test -p <member>` still target);
/// - declares `[lib] name = "<member>" path = "<abs>/src/lib.rs"` so
///   `extern crate <member>;` in the aggregator's `src/main.rs` resolves
///   to the bridge's rlib (renamed via `package =` in the aggregator's
///   `[dependencies]`) and the compilation unit is the REAL src tree;
/// - carries `build = "<abs>/build.rs"` when the real member has one,
///   so build-script env vars (e.g. `rustc-env=SOMETHING=...`) still
///   fire at the bridge's compilation;
/// - merges `[dependencies]` ∪ `[dev-dependencies]` ∪ the target-cfg
///   variants of both into a single `[dependencies]` table, so
///   `use ::rudzio::...` + any fake/mockito/serde-json etc. in src
///   tests resolve when cargo compiles under `--cfg rudzio_test`;
/// - carries an empty `[workspace]` stanza so the bridge becomes its
///   own workspace root (the enclosing rudzio-auto-runner aggregator is
///   also its own workspace, and cargo rejects nested workspaces that
///   share `[workspace]` tables).
///
/// The bridge dir itself does NOT get a `src/lib.rs` placeholder —
/// explicit `[lib] path = ...` tells cargo to look only at that path,
/// so an empty dir with just a `Cargo.toml` is enough.
///
/// # Errors
///
/// Returns an error when the bridge directory cannot be wiped /
/// recreated, when the bridge `Cargo.toml` / `build.rs` cannot be
/// emitted, or when symlinking the member tree fails.
#[inline]
pub fn write_bridge_crate(plan: &Plan, member: &MemberPlan, out: &Path) -> Result<()> {
    let bridge_dir = out.join(bridge_dir_name(member));
    // Wipe any pre-existing bridge directory so stale symlinks from a
    // prior run (member entry renamed or removed) don't survive.
    if bridge_dir.exists() {
        fs::remove_dir_all(&bridge_dir)
            .with_context(|| format!("wiping stale bridge dir {}", bridge_dir.display()))?;
    }
    fs::create_dir_all(&bridge_dir)
        .with_context(|| format!("creating bridge dir {}", bridge_dir.display()))?;
    let manifest = build_bridge_cargo_toml(plan, member)?;
    fs::write(bridge_dir.join("Cargo.toml"), manifest)
        .with_context(|| format!("writing bridge Cargo.toml at {}", bridge_dir.display()))?;
    fs::write(bridge_dir.join("build.rs"), build_bridge_build_rs(member))
        .with_context(|| format!("writing bridge build.rs at {}", bridge_dir.display()))?;
    symlink_member_tree_into_bridge(&member.manifest_dir, &bridge_dir)?;
    Ok(())
}

/// Symlink every non-skiplist top-level entry of the member's manifest
/// dir into the bridge dir. This makes `CARGO_MANIFEST_DIR`-relative
/// path lookups (e.g. `include_str!("data.json")`,
/// `sqlx::migrate!("migrations")`) resolve transparently under bridge
/// compile — the bridge dir becomes a structural twin of the member's
/// root, minus the Cargo.toml / build.rs that the bridge overrides.
///
/// If the member's `manifest_dir` is unreadable (e.g. synthetic test
/// fixtures using a fake path), the symlink pass is a no-op. Bridge
/// compile would fail anyway in that case — the no-op keeps synthetic
/// unit tests of Cargo.toml emission unaffected.
fn symlink_member_tree_into_bridge(member_dir: &Path, bridge_dir: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(member_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if BRIDGE_SKIPLIST.contains(&name_str) {
            continue;
        }
        let source = entry.path();
        let target = bridge_dir.join(&name);
        create_symlink(&source, &target).with_context(|| {
            format!(
                "symlinking bridge entry {} -> {}",
                target.display(),
                source.display()
            )
        })?;
    }
    Ok(())
}

/// Create a filesystem symlink from `source` to `target` (Unix variant).
///
/// `AlreadyExists` is treated as success when the existing symlink
/// already points to `source`. This makes the bridge-symlinking step
/// idempotent under concurrent same-workspace invocations: invocation
/// A creates the symlink between invocation B's `Path::exists()` check
/// (sees: no) and B's `symlink()` syscall (sees: EEXIST). The original
/// behaviour surfaced that race as
/// `symlinking bridge entry ... -> ...: File exists (os error 17)`;
/// validating the existing target matches the requested source keeps
/// the operation safe without introducing a lock.
///
/// A genuine conflict (existing symlink points to a DIFFERENT source)
/// still errors with the original `AlreadyExists` so the user sees
/// the inconsistent state rather than silent overwrite.
#[cfg(unix)]
fn create_symlink(source: &Path, target: &Path) -> IoResult<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::symlink;
    match symlink(source, target) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => match fs::read_link(target) {
            Ok(existing) if existing == source => Ok(()),
            Ok(_) | Err(_) => Err(err),
        },
        Err(err) => Err(err),
    }
}

/// Create a filesystem symlink from `source` to `target` (Windows stub).
#[cfg(windows)]
fn create_symlink(_source: &Path, _target: &Path) -> IoResult<()> {
    use std::io::{Error, ErrorKind};
    Err(Error::new(
        ErrorKind::Unsupported,
        "cargo-rudzio bridge symlinking is not yet implemented on Windows",
    ))
}

/// `fs::remove_dir_all` that treats `NotFound` as success.
///
/// Same race-tolerance rationale as
/// [`remove_file_tolerating_missing`]: under concurrent same-workspace
/// invocations, one invocation may have already removed the directory
/// between this invocation's `exists()` check and its remove call.
/// Treating `NotFound` as a no-op keeps the cleanup phase idempotent
/// without locking.
fn remove_dir_all_tolerating_missing(path: &Path) -> IoResult<()> {
    use std::io::ErrorKind;
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// `fs::remove_file` that treats `NotFound` as success.
///
/// Required for safety under concurrent `cargo rudzio test`
/// invocations against the same workspace (and thus the same
/// `aggregator_subdir_name`). Two invocations enter `write_runner`
/// roughly simultaneously, both pass the `path.exists()` guard, then
/// invocation A's `fs::remove_file` succeeds and invocation B's hits
/// `ENOENT`. Without this helper the original code surfaced that race
/// as `cargo-rudzio: removing .../Cargo.toml: No such file or
/// directory` even though both invocations agree on what the post-
/// cleanup state should be: the file is gone, both can proceed.
fn remove_file_tolerating_missing(path: &Path) -> IoResult<()> {
    use std::io::ErrorKind;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Synthesize the bridge's `build.rs`.
///
/// Every bridge gets one — it is the per-compile-unit emission site for
/// `cargo:rustc-cfg=rudzio_test` that scopes the cfg to the bridge's
/// rustc invocation only (as opposed to propagating it through ambient
/// `RUSTFLAGS`, which leaked into nested `cargo build --bins` and
/// caused thousands of unresolved-crate errors in downstream bin
/// crates).
///
/// For bin-bearing members the build.rs additionally invokes
/// `expose_member_bins` so `cargo:rustc-env=CARGO_BIN_EXE_<bin>=<abs>`
/// reaches the bridge's compile unit, where `rudzio::bin!(...)`
/// ultimately expands. The member's own `build.rs` is never forwarded:
/// its emissions would scope to the member's standalone compile unit,
/// but the cfg needs to land on the bridge.
#[inline]
#[must_use]
pub fn build_bridge_build_rs(member: &MemberPlan) -> String {
    // The bridge's own manifest dir lives under
    // `target/<...>/members/<member>/`, but path-resolving proc-macros
    // (`include_str!`, `sqlx::migrate!`, `refinery::embed_migrations!`,
    // askama templates, …) read `CARGO_MANIFEST_DIR` and look for
    // files relative to it. Without this override, anything outside
    // the symlink-mirrored top-level (e.g. `..`-relative paths to a
    // sibling crate's shared resources) fails to resolve. Emitting
    // `cargo:rustc-env=CARGO_MANIFEST_DIR=<original member dir>` makes
    // the bridge's rustc see the same manifest dir cargo would set
    // under stock per-crate builds, so member src compiles identically
    // inside and outside the aggregator.
    let manifest_dir = escape_str_literal(&member.manifest_dir.to_string_lossy());
    let mut out = String::new();
    if member.bin_names.is_empty() {
        out.push_str("fn main() {\n");
        out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
        out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
        let _ignored: FmtResult = writeln!(
            out,
            "    println!(\"cargo:rustc-env=CARGO_MANIFEST_DIR={manifest_dir}\");"
        );
        out.push_str("}\n");
        return out;
    }
    out.push_str(BUILD_RS_HELPERS);
    out.push_str("\nfn main() -> Result<(), String> {\n");
    out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
    out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
    let _manifest_emit: FmtResult = writeln!(
        out,
        "    println!(\"cargo:rustc-env=CARGO_MANIFEST_DIR={manifest_dir}\");"
    );
    let _expose_emit: FmtResult = write!(
        out,
        "    expose_member_bins({}, {}, &[",
        quote_str(&member.package_name),
        quote_str(&member.manifest_dir.to_string_lossy()),
    );
    for (idx, bin) in member.bin_names.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&quote_str(bin));
    }
    out.push_str("])?;\n    Ok(())\n}\n");
    out
}

/// Cargo `[package] name` to use for `member`'s bridge crate.
#[inline]
#[must_use]
pub fn bridge_package_name(member: &MemberPlan) -> String {
    format!(
        "{}_rudzio_bridge",
        crate_name_to_ident(&member.package_name)
    )
}

/// Subdirectory name (under the aggregator's `members/` dir) that holds the bridge for `member`.
#[inline]
#[must_use]
pub fn bridge_dir_name(member: &MemberPlan) -> String {
    crate_name_to_ident(&member.package_name)
}

/// Render the bridge crate's `Cargo.toml` for `member` as a TOML document string.
///
/// Mirrors the merged `[dependencies]` ∪ `[dev-dependencies]` of the
/// member, points `[lib] path` at the real `src/lib.rs`, and synthesises
/// a `build.rs` that emits `cargo:rustc-cfg=rudzio_test`.
///
/// # Errors
///
/// Returns an error when the member has no `src_lib_path`, or when
/// rendering any dependency entry into TOML fails.
#[inline]
pub fn build_bridge_cargo_toml(plan: &Plan, member: &MemberPlan) -> Result<String> {
    let lib_path = member.src_lib_path.as_ref().ok_or_else(|| {
        anyhow!(
            "bridge requested for `{}` but the member has no [lib] target",
            member.package_name
        )
    })?;

    let mut doc = DocumentMut::new();

    let mut pkg = Table::new();
    pkg.insert("name", value(bridge_package_name(member)));
    pkg.insert("version", value("0.0.0"));
    pkg.insert("edition", value(member.edition.as_str()));
    pkg.insert("publish", value(false));
    // Every bridge gets its own synthesised build.rs that emits
    // `cargo:rustc-cfg=rudzio_test` (and, for bin members, exposes the
    // member's bins via `expose_member_bins`). The member's own
    // build.rs is never forwarded — its emissions would scope to the
    // member's standalone compile unit, but the cfg needs to land on
    // the bridge's compile unit instead.
    pkg.insert("build", value("build.rs"));
    doc.insert("package", Item::Table(pkg));

    // No `[workspace]` stanza here: the bridge deliberately attaches to
    // the enclosing aggregator's workspace (which lists the bridge under
    // `[workspace] members`). Declaring a second `[workspace]` would
    // produce nested workspaces, which cargo rejects ("multiple
    // workspace roots found").

    let mut lib = Table::new();
    lib.insert("name", value(crate_name_to_ident(&member.package_name)));
    // Relative path; the bridge dir symlinks the member's top-level
    // entries, so resolving `<rel>/lib.rs` inside the bridge dir lands
    // on the member's real source. Path-based macros like
    // `include_str!` / `sqlx::migrate!` resolve against
    // `CARGO_MANIFEST_DIR` (= the bridge dir) and likewise reach the
    // member tree through the symlinks.
    let rel_lib = lib_path
        .strip_prefix(&member.manifest_dir)
        .map_or_else(|_| PathBuf::from("src").join("lib.rs"), Path::to_path_buf);
    let rel_lib_str = rel_lib.to_string_lossy().replace('\\', "/");
    lib.insert("path", value(rel_lib_str));
    doc.insert("lib", Item::Table(lib));

    let deps_tbl = build_bridge_dependencies(plan, member)?;
    doc.insert("dependencies", Item::Table(deps_tbl));

    // No [build-dependencies]: the synth build.rs uses only std
    // (PathBuf, Command, std::env). External crates the member's
    // own build.rs may have needed are not relevant here because
    // we don't forward that build.rs.

    if let Some(features_tbl) = build_bridge_features(plan, member) {
        doc.insert("features", Item::Table(features_tbl));
    }

    // [lints.rust] block registers `cfg(rudzio_test)` so member src
    // that uses `cfg_attr(any(test, rudzio_test), ...)` doesn't draw
    // `unexpected cfg condition name: rudzio_test` warnings. The
    // bridge's build.rs already emits
    // `cargo::rustc-check-cfg=cfg(rudzio_test)`, but in practice
    // downstream cargo/rustc combinations have produced the warning
    // anyway during the metadata / pre-build phase — likely against
    // the member's original Cargo.toml before the bridge build script
    // runs. Belt-and-suspenders: declare the cfg directly in the
    // bridge manifest where rustc can't miss it.
    let mut lints_rust = Table::new();
    let mut unexpected_cfgs = InlineTable::new();
    unexpected_cfgs.insert("level", Value::String(Formatted::new("warn".to_owned())));
    let mut check_cfg = Array::new();
    check_cfg.push(Value::String(Formatted::new("cfg(rudzio_test)".to_owned())));
    unexpected_cfgs.insert("check-cfg", Value::Array(check_cfg));
    lints_rust.insert(
        "unexpected_cfgs",
        Item::Value(Value::InlineTable(unexpected_cfgs)),
    );
    let mut lints_tbl = Table::new();
    lints_tbl.insert("rust", Item::Table(lints_rust));
    doc.insert("lints", Item::Table(lints_tbl));

    Ok(doc.to_string())
}

/// Build the bridge's `[features]` table. Mirrors the member's own
/// `[features]` 1:1, then overwrites the `default` entry with the
/// sorted, deduped union of (member's own `default`) ∪
/// (`rudzio_activated_features`) ∪ (every feature any sibling
/// workspace member activates on `member` via its `[dependencies]`
/// or `[dev-dependencies]`). Returns `None` when every source is
/// empty — no `[features]` section is emitted in that case.
///
/// Why the sibling union: under stock cargo, when consumer A says
/// `B = { features = ["mock"] }`, cargo activates `mock` on B by
/// feature unification across the dep graph. The bridge generator
/// produces multiple compile units of B (the bridge crate plus the
/// aggregator's own path-only reference), and unification only
/// activates a feature when SOMEONE asks for it. Folding consumer
/// requests into the bridge's `default` makes the activation
/// independent of which compile-unit cargo happens to drive feature
/// resolution from, and removes the need for members to carry a
/// `[package.metadata.rudzio] features = [...]` workaround that
/// duplicates information already present in consumer manifests.
fn build_bridge_features(plan: &Plan, member: &MemberPlan) -> Option<Table> {
    let consumer_requests = collect_sibling_feature_requests(plan, member);
    if member.features.is_empty()
        && member.rudzio_activated_features.is_empty()
        && consumer_requests.is_empty()
    {
        return None;
    }

    let mut tbl = Table::new();
    for (key, values) in &member.features {
        if key == "default" {
            continue;
        }
        let mut arr = Array::new();
        for feat in values {
            arr.push(feat.as_str());
        }
        tbl.insert(key, value(arr));
    }

    let mut default: BTreeSet<String> = BTreeSet::new();
    if let Some(member_default) = member.features.get("default") {
        for feat in member_default {
            let _default_inserted: bool = default.insert(feat.clone());
        }
    }
    for feat in &member.rudzio_activated_features {
        let _default_inserted: bool = default.insert(feat.clone());
    }
    for feat in consumer_requests {
        let _default_inserted: bool = default.insert(feat);
    }

    let mut default_arr = Array::new();
    for feat in &default {
        default_arr.push(feat.as_str());
    }
    tbl.insert("default", value(default_arr));

    Some(tbl)
}

/// Walk every other member's `dev_deps` (which carries both regular
/// `[dependencies]` and test-only `[dev-dependencies]`) and collect
/// the features they activate on `member`. Workspace-inherited
/// requests pull in the workspace dep's own features in addition to
/// any per-consumer overrides.
fn collect_sibling_feature_requests(plan: &Plan, member: &MemberPlan) -> BTreeSet<String> {
    let mut features: BTreeSet<String> = BTreeSet::new();
    for sibling in &plan.members {
        if sibling.package_name == member.package_name {
            continue;
        }
        for dep in &sibling.dev_deps {
            if dep.name != member.package_name {
                continue;
            }
            if dep.workspace_inherited
                && let Some(ws) = plan.workspace_deps.get(&dep.name)
            {
                for feat in &ws.features {
                    let _inserted: bool = features.insert(feat.clone());
                }
            }
            for feat in &dep.features {
                let _inserted: bool = features.insert(feat.clone());
            }
        }
    }
    features
}

/// Merge the member's `[dependencies]` + `[dev-dependencies]` + both
/// target-cfg variants into one flat `[dependencies]` table for the
/// bridge, plus inject `rudzio` if the member didn't already declare
/// it. Anyhow is intentionally NOT injected: rudzio's void-fn rewrite
/// uses `::rudzio::BoxError` (defined in rudzio itself) as the error
/// type, so no anyhow dependency leaks onto users through the bridge.
fn build_bridge_dependencies(plan: &Plan, member: &MemberPlan) -> Result<Table> {
    let mut deps = Table::new();

    let mut merged: BTreeMap<String, DevDepSpec> = BTreeMap::new();
    for dep in &member.dev_deps {
        // Skip self-references in the member's own [dev-dependencies]
        // (typical of dogfooding setups, e.g. rudzio's own root manifest
        // declares `rudzio = { path = "." }` to test itself). Without
        // this guard, the sibling-bridge redirect in `render_dev_dep`
        // would point the bridge at its own package and cargo halts with
        // a `cyclic package dependency` error. The rudzio-injection
        // block below fills the entry back in from the workspace spec.
        if dep.name == member.package_name {
            continue;
        }
        let entry_name = dep.rename.as_deref().unwrap_or(&dep.name).to_owned();
        merged
            .entry(entry_name)
            .and_modify(|existing| {
                for feat in &dep.features {
                    if !existing.features.contains(feat) {
                        existing.features.push(feat.clone());
                    }
                }
                existing.uses_default_features &= dep.uses_default_features;
                existing.section = existing.section.merge(dep.section);
            })
            .or_insert_with(|| dep.clone());
    }

    for (entry_name, dep) in &merged {
        // Skip dev-only deps that name another bridged sibling: re-emitting
        // them in the bridge's [dependencies] would form a cycle once
        // sibling-bridge redirection rewrites them to the sibling's bridge
        // package, and cargo halts with `error: cyclic package dependency`.
        // The aggregator's own [dependencies] already pulls every bridged
        // sibling in, so test-time uses of the dep still resolve.
        if dep.section.is_dev_only()
            && plan
                .members
                .iter()
                .any(|sibling| sibling.package_name == dep.name && bridge_applies_to(sibling))
        {
            continue;
        }
        let item = render_dev_dep(dep, plan)?;
        deps.insert(entry_name, item);
    }

    // Bridges exist to expose dev-deps under `--cfg rudzio_test`, and
    // rudzio itself is the universally-required one. If the member
    // declared rudzio only under `[dev-dependencies]` the merge above
    // already surfaces it; if the member didn't declare rudzio at all
    // (defensive — we already filter to rudzio-using members) we inject
    // the aggregator's unified spec so `use ::rudzio::*` still resolves.
    //
    // Skip the inject for rudzio's own bridge: a `rudzio` dep on the
    // rudzio bridge would either self-loop (after sibling-bridge
    // redirection) or pull in `/rudzio` as a second compile unit
    // alongside the bridge, leading to duplicate `#[distributed_slice]`
    // registrations at runtime. Rudzio's own src uses `crate::*` for
    // self-references; an extern `rudzio` crate isn't needed.
    if member.package_name != RUDZIO_DEP && !deps.contains_key(RUDZIO_DEP) {
        let tbl = build_rudzio_inline_table(&plan.rudzio_spec);
        deps.insert(RUDZIO_DEP, Item::Value(Value::InlineTable(tbl)));
    }

    Ok(deps)
}

/// Build the aggregator's `src/main.rs`.
///
/// Emits `extern crate <crate>;` for every member we also list as a
/// path dep in the aggregator's `[dependencies]`, which forces rustc
/// to actually link each member's rlib into the final binary. Without
/// this, rustc drops unreferenced rlibs during link-time DCE and
/// linkme's `#[link_section]` statics in those crates (even annotated
/// `#[used]`, which only blocks in-object DCE) never reach the
/// binary — meaning `#[rudzio::test]` fns under a member's `src/**`
/// wouldn't be discovered at run time. The member package name is
/// normalised hyphens→underscores to match cargo's own
/// crate-name-to-ident rule.
#[inline]
#[must_use]
pub fn build_main_rs(plan: &Plan, out_dir: &Path) -> String {
    let mut out = String::from(
        "#![allow(
    non_snake_case,
    unsafe_code,
    unreachable_pub,
    unused_crate_dependencies,
    unused_extern_crates,
    clippy::tests_outside_test_module,
    clippy::single_component_path_imports,
    reason = \"auto-generated rudzio test aggregator\"
)]

",
    );

    let workspace_root_abs = plan.workspace_root.as_std_path();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for member in &plan.members {
        if paths_equal(&member.manifest_dir, workspace_root_abs) {
            continue;
        }
        if !member.has_lib {
            continue;
        }
        let ident = crate_name_to_ident(&member.package_name);
        if !seen.insert(ident.clone()) {
            continue;
        }
        let _ignored: FmtResult = writeln!(out, "extern crate {ident};");
    }

    // Transparent re-export of each member's test-root helper modules
    // at the aggregator's crate root. Under stock cargo, a `[[test]]
    // <name>` binary with `tests/<name>/mod.rs` as its entry point has
    // mod.rs as the crate root, so `mod common;` in mod.rs makes
    // `crate::common` resolve from any sibling test file. Under the
    // aggregator mod.rs is `#[path]`-included as a NESTED module, and
    // `crate::common` from inside it looks at the (different)
    // aggregator crate root. Mirroring those helper-mod declarations
    // here at the actual crate root restores stock-cargo semantics.
    //
    // Collisions across members (two members declaring `mod common;`
    // with different files) are flagged by `collect_test_root_helpers`
    // so the user gets a clear error rather than a silent first-wins.
    for helper in collect_test_root_helpers(plan, out_dir) {
        let _ignored: FmtResult = writeln!(
            out,
            "#[path = {}]\nmod {};",
            quote_str(&helper.path.to_string_lossy()),
            helper.module,
        );
    }

    // The tests module gets a unique name (`__rudzio_member_tests`)
    // rather than the natural `tests`. `rudzio::manifest_dir!()`
    // identifies aggregator-context module paths by looking for that
    // marker segment — the natural `tests` would collide with member
    // src test mods (`mod tests` inside a bridged member's src), and
    // the resolver couldn't distinguish them.
    out.push_str(
        "
#[path = \"tests.rs\"]
mod __rudzio_member_tests;

#[rudzio::main]
fn main() {",
    );

    // Wire the per-member `RUDZIO_MEMBER_MANIFEST_DIR_<n>=<abs>` env
    // vars build.rs emitted into the rudzio runtime registry so
    // `rudzio::manifest_dir!()` can resolve them at the test's call
    // site. `#[rudzio::main]` inlines this body before the runner.
    let test_members: Vec<&MemberPlan> = plan
        .members
        .iter()
        .filter(|member| !member.test_files.is_empty())
        .collect();
    if test_members.is_empty() {
        out.push_str("}\n");
        return out;
    }
    out.push_str("\n    ::rudzio::member_meta::register_member_manifest_dirs(&[\n");
    for member in test_members {
        let ident = sanitize_ident(&member.package_name);
        let _ignored: FmtResult = writeln!(
            out,
            "        ({}, env!(\"RUDZIO_MEMBER_MANIFEST_DIR_{ident}\")),",
            quote_str(&ident)
        );
    }
    out.push_str("    ]);\n}\n");
    out
}

/// Walk every member's test-root files, parse each for top-level
/// `mod <name>;` declarations, and emit one [`TestRootHelper`] per
/// `(test_binary, helper)` pair. Each helper gets a synthesised
/// crate-root module name that is unique across the workspace, so
/// two `[[test]]` binaries declaring `mod common;` against different
/// sibling files no longer collide (Bug #10 — the previous
/// implementation `BTreeMap`-deduplicated by bare module name, which
/// silently first-wins). Source files in each test binary go through
/// a syn-based rewrite so `use crate::<helper>` inside those files
/// resolves to the binary-scoped helper, not whichever helper the
/// first-wins lottery favoured.
fn collect_test_root_helpers(plan: &Plan, out_dir: &Path) -> Vec<TestRootHelper> {
    collect_test_binary_proxies(plan, out_dir)
        .into_iter()
        .flat_map(|proxy| proxy.helpers_at_crate_root.into_iter())
        .collect()
}

/// Bug #10 collision detection: when two `[[test]]` binaries declare
/// `mod <name>;` decls that resolve to DIFFERENT source HELPER
/// files, emit a single error chain naming every offender (helper
/// name, each binary id, each source path). Same-helper-name
/// pointing at the SAME canonical file is deduplicated (legitimate
/// shared helper via `#[path]` — Bug #8B edge).
///
/// **Test files are exempt.** A `mod foo;` declaration that resolves
/// to a file containing `#[rudzio::test]` / `#[rudzio::suite]` is a
/// per-binary test file, NOT a helper — different binaries
/// naturally have differently-named-but-same-stem test files
/// (`a/foo.rs` vs `b/foo.rs`) without that being a user error. The
/// per-binary synth-name scoping handles them automatically.
/// Only helpers (files without test markers) are subject to the
/// strict collision rule.
///
/// # Errors
///
/// Returns an error whose context chain lists each colliding helper
/// name and every binary/file pair involved.
fn detect_helper_collisions(plan: &Plan) -> Result<()> {
    // helper_name → distinct canonical paths → list of binary_ids
    // referencing that path. Two distinct canonical paths under the
    // same helper_name == collision.
    let mut by_helper: BTreeMap<String, BTreeMap<PathBuf, BTreeSet<String>>> = BTreeMap::new();
    for member in &plan.members {
        for test_file in &member.test_files {
            let binary_id = compute_test_binary_id(member, test_file);
            for (helper_name, source_path) in discover_test_root_helpers(test_file) {
                if file_carries_test_markers(&source_path) {
                    continue;
                }
                let canon = canonicalize_or_keep(&source_path);
                let _newly_inserted: bool = by_helper
                    .entry(helper_name)
                    .or_default()
                    .entry(canon)
                    .or_default()
                    .insert(binary_id.clone());
            }
        }
    }
    let mut collision_lines: Vec<String> = Vec::new();
    for (helper_name, paths_to_bins) in &by_helper {
        if paths_to_bins.len() < 2 {
            continue;
        }
        // Build a "helper `common`:\n  bin a → path X\n  bin b → path Y" block.
        let mut block = format!(
            "helper `{helper_name}` resolves to {n} distinct files across binaries:",
            n = paths_to_bins.len()
        );
        for (canon, bins) in paths_to_bins {
            for bin_id in bins {
                let _written: FmtResult = write!(
                    block,
                    "\n  binary `{bin_id}` → {path}",
                    path = canon.display()
                );
            }
        }
        collision_lines.push(block);
    }
    if collision_lines.is_empty() {
        return Ok(());
    }
    bail!(
        "Bug #10 helper collisions detected. Rename one of the helper modules, \
         or use `#[path = \"…\"]` on the `mod` decls to point them at the same \
         file when sharing is intended.\n\n{}",
        collision_lines.join("\n\n"),
    );
}

/// `true` when the file at `path` carries `#[rudzio::test]` /
/// `#[rudzio::suite]` markers (substring scan, tolerates absolute
/// paths like `#[::rudzio::test]`). Used by collision detection to
/// distinguish per-binary test files from cross-binary helpers.
/// Read failures (missing file, permission denied) return `false`
/// — defensive: an unreadable file can't be confirmed to be a test
/// file, so the conservative answer is "treat as helper" and let
/// the strict collision rule fire.
fn file_carries_test_markers(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.contains("rudzio::test") || text.contains("rudzio::suite")
}

/// Walk every member's `test_files`, build a [`TestBinaryProxy`] for
/// each `[[test]]` entry, and discover the helpers it declares.
/// Pure: no filesystem writes. The proxy paths returned are the
/// paths the on-disk proxies WILL live at after
/// [`write_test_binary_proxies`] runs; `build_main_rs` /
/// `build_tests_rs` use those paths in `#[path = ...]` attributes
/// regardless of whether they exist yet.
fn collect_test_binary_proxies(plan: &Plan, out_dir: &Path) -> Vec<TestBinaryProxy> {
    let proxies_root = out_dir.join("src").join("proxies");
    // `<out_dir>/src/proxies/<id>` is the canonical proxy location.
    // `write_test_binary_proxies` writes files there and main.rs /
    // tests.rs reference them via `#[path]`. Both must agree on the
    // path, so `out_dir` is threaded through every collector +
    // builder rather than recomputed from `plan.workspace_root`
    // (which would force the default-output-dir convention onto
    // callers that pass a custom out_dir, e.g. tempdirs in tests).
    let mut out: Vec<TestBinaryProxy> = Vec::new();
    for member in &plan.members {
        for test_file in &member.test_files {
            let Some(test_root_dir) = test_file.parent() else {
                continue;
            };
            let binary_id = compute_test_binary_id(member, test_file);
            let proxy_dir = proxies_root.join(&binary_id);
            let proxy_test_root = match test_file.file_name() {
                Some(name) => proxy_dir.join(name),
                None => continue,
            };
            let helpers_raw = discover_test_root_helpers(test_file);
            let mut helper_synth_lookup: BTreeMap<String, String> = BTreeMap::new();
            let mut helpers_at_crate_root: Vec<TestRootHelper> = Vec::new();
            for (helper_name, source_path) in helpers_raw {
                let synth = format!("__rudzio__{binary_id}__{helper_name}");
                let proxy_path = match source_path.strip_prefix(test_root_dir) {
                    Ok(rel) => proxy_dir.join(rel),
                    Err(_) => continue,
                };
                helpers_at_crate_root.push(TestRootHelper {
                    module: synth.clone(),
                    path: proxy_path,
                });
                let _previous_synth = helper_synth_lookup.insert(helper_name, synth);
            }
            out.push(TestBinaryProxy {
                binary_id,
                helpers_at_crate_root,
                helper_synth_lookup,
                test_root: test_file.clone(),
                test_root_dir: test_root_dir.to_path_buf(),
                proxy_test_root,
            });
        }
    }
    out
}

/// Stable per-test-binary id: `<sanitize(member_pkg)>__<sanitize(parent_dir_name)>`.
///
/// Combines the member package name and the test-root's parent
/// directory name so two binaries inside the same crate
/// (`tests/integration/mod.rs` + `tests/e2e/mod.rs`) get distinct
/// ids without conflict, and the same binary across two members
/// (each named `tests/integration/mod.rs`) also stays distinct.
/// Flat single-file test files (`tests/foo.rs`, no parent dir per se
/// — parent is `tests/`) fall back to the file stem.
fn compute_test_binary_id(member: &MemberPlan, test_file: &Path) -> String {
    let member_part = sanitize_ident(&member.package_name);
    let parent_part = if test_file.file_name().and_then(|name| name.to_str()) == Some("mod.rs") {
        test_file
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("root")
    } else {
        test_file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("test")
    };
    format!("{member_part}__{}", sanitize_ident(parent_part))
}

/// Write the per-test-binary proxy files under
/// `<out_dir>/src/proxies/<binary_id>/`. Each proxy is a copy of the
/// original source with two transformations:
///
/// * `mod <helper>;` declarations in the test-root mod.rs are
///   stripped (the helper is hoisted to the aggregator's crate root
///   under a synthesised name; leaving the declaration in mod.rs
///   would compile the helper twice and produce duplicate-type errors
///   when something passes one through API expecting the other).
///
/// * `crate::<helper>` paths in every file are rewritten to
///   `crate::<synth_name>` via a syn `visit_mut::VisitMut` walker,
///   then re-emitted with `prettyplease`. Correct under every Rust
///   syntax (comments, raw strings, macros).
///
/// # Errors
///
/// Returns an error when any source file fails to parse, any proxy
/// file fails to write, or any directory creation fails. Read-fail
/// on an individual source file is fatal so missing helpers don't
/// silently degrade to a "first source wins" cascade.
fn write_test_binary_proxies(out_dir: &Path, proxies: &[TestBinaryProxy]) -> Result<()> {
    let proxies_root = out_dir.join("src").join("proxies");
    if proxies_root.exists() {
        fs::remove_dir_all(&proxies_root)
            .with_context(|| format!("removing {}", proxies_root.display()))?;
    }
    for proxy in proxies {
        let proxy_dir = proxies_root.join(&proxy.binary_id);
        fs::create_dir_all(&proxy_dir)
            .with_context(|| format!("creating {}", proxy_dir.display()))?;
        let source_files = walk_rs_files(&proxy.test_root_dir);
        let helper_names: BTreeSet<String> = proxy.helper_synth_lookup.keys().cloned().collect();
        for source_file in source_files {
            let rel = source_file
                .strip_prefix(&proxy.test_root_dir)
                .with_context(|| {
                    format!(
                        "strip-prefix {} from {}",
                        proxy.test_root_dir.display(),
                        source_file.display()
                    )
                })?;
            let dest = proxy_dir.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let original = fs::read_to_string(&source_file)
                .with_context(|| format!("reading {}", source_file.display()))?;
            let rewritten = if source_file == proxy.test_root {
                let stripped = strip_helper_mod_decls(&original, &helper_names)
                    .with_context(|| format!("stripping mod decls in {}", source_file.display()))?;
                rewrite_crate_paths(&stripped, &proxy.helper_synth_lookup).with_context(|| {
                    format!("rewriting crate paths in {}", source_file.display())
                })?
            } else {
                rewrite_crate_paths(&original, &proxy.helper_synth_lookup).with_context(|| {
                    format!("rewriting crate paths in {}", source_file.display())
                })?
            };
            fs::write(&dest, rewritten).with_context(|| format!("writing {}", dest.display()))?;
        }
    }
    Ok(())
}

/// Depth-first walk of `dir`, returning every `.rs` file inside it
/// (recursive). Used to enumerate the full source set of a test
/// binary so the proxy emit covers every file `mod.rs` reaches
/// transitively. Errors during traversal are dropped: a unreadable
/// subdir yields fewer files (which surfaces later as an unresolved
/// mod), but doesn't abort the whole proxy emission.
fn walk_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry_result in entries {
        let Ok(entry) = entry_result else { continue };
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_rs_files(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        } else {
            // Non-Rust files (`*.toml`, `*.md`, build-script
            // outputs, etc.) are ignored — they don't contribute
            // test-binary sources and the proxy emit shouldn't copy
            // them. Explicit branch silences the `else_if_without_else`
            // lint without changing behaviour.
        }
    }
    out.sort();
    out
}

/// Parse `source` with syn, rewrite every `crate::<helper>::...` path
/// to `crate::<synth>::...` where the substitution is given by
/// `helpers` (`helper_name -> synth_name`), and re-emit via
/// `prettyplease`. AST-level (not regex) so raw strings, comments,
/// and macros cannot trick the substitution.
///
/// # Errors
///
/// Returns an error when `source` is not valid Rust — the
/// `prettyplease::unparse` step itself is infallible.
fn rewrite_crate_paths(source: &str, helpers: &BTreeMap<String, String>) -> Result<String> {
    if helpers.is_empty() {
        return Ok(source.to_owned());
    }
    let mut file: syn::File = syn::parse_str(source).context("parsing test-binary source file")?;
    let mut rewriter = CratePathRewriter { helpers };
    rewriter.visit_file_mut(&mut file);
    Ok(prettyplease::unparse(&file))
}

/// Parse `source` with syn, drop every top-level `mod <name>;`
/// declaration whose `<name>` is in `helpers`, and re-emit via
/// `prettyplease`. Used on the proxy test-root mod.rs so the helpers
/// hoisted to the aggregator's crate root don't get redeclared
/// nested (which would compile them twice as distinct types).
///
/// # Errors
///
/// Returns an error when `source` is not valid Rust.
fn strip_helper_mod_decls(source: &str, helpers: &BTreeSet<String>) -> Result<String> {
    if helpers.is_empty() {
        return Ok(source.to_owned());
    }
    let mut file: syn::File = syn::parse_str(source).context("parsing test-root mod.rs")?;
    file.items.retain(|item| {
        if let syn::Item::Mod(mod_item) = item
            && mod_item.content.is_none()
            && helpers.contains(&mod_item.ident.to_string())
        {
            false
        } else {
            true
        }
    });
    Ok(prettyplease::unparse(&file))
}

/// Rewrite the first identifier inside a `UseTree` when it matches a
/// helper-name key. Called after detecting the `crate::` head; the
/// first ident OF THE SUB-TREE is the helper name (e.g. for
/// `use crate::common::X`, the sub-tree starts at `common`).
///
/// Handles every `UseTree` variant cargo accepts after the head
/// segment: another `Path` (`crate::common::X`), a leaf `Name`
/// (`crate::common`), a `Rename` (`crate::common as foo`), a `Group`
/// (`crate::{common, other}`), or `Glob` (`crate::*` — no helper
/// match possible).
///
/// **Bare leaf `Name`**: a `use crate::common;` import binds `common`
/// in the surrounding scope. Naively replacing the ident with the
/// synth name would change the binding to
/// `__rudzio__<member>__<bin>__common`, breaking every body
/// reference that spells the helper as `common`. The fix promotes the
/// `Name` variant to a `Rename`, emitting
/// `use crate::__rudzio__...__common as common;` so the local binding
/// survives the rewrite (Bug #12). The same promotion runs for each
/// `Group` item by recursion, so `use crate::{common, other};` aliases
/// each matching leaf independently.
fn rewrite_first_use_tree_ident(tree: &mut syn::UseTree, helpers: &BTreeMap<String, String>) {
    match tree {
        syn::UseTree::Path(path) => {
            if let Some(synth) = helpers.get(&path.ident.to_string()) {
                path.ident = syn::Ident::new(synth, path.ident.span());
            }
        }
        syn::UseTree::Name(name) => {
            if let Some(synth) = helpers.get(&name.ident.to_string()) {
                let original = name.ident.clone();
                let span = name.ident.span();
                let synth_ident = syn::Ident::new(synth, span);
                *tree = syn::UseTree::Rename(syn::UseRename {
                    ident: synth_ident,
                    as_token: syn::Token![as](span),
                    rename: original,
                });
            }
        }
        syn::UseTree::Rename(rename) => {
            if let Some(synth) = helpers.get(&rename.ident.to_string()) {
                rename.ident = syn::Ident::new(synth, rename.ident.span());
            }
        }
        syn::UseTree::Group(group) => {
            for item in &mut group.items {
                rewrite_first_use_tree_ident(item, helpers);
            }
        }
        syn::UseTree::Glob(_) => {}
    }
}

/// Normalise a Cargo package name (which allows hyphens) into the Rust
/// identifier cargo actually uses for `extern crate` / path references.
fn crate_name_to_ident(name: &str) -> String {
    name.replace('-', "_")
}

/// Parse `test_file` (the `[[test]] path = "..."` entry-point of a
/// member's integration test binary) for top-level `mod <name>;`
/// declarations and resolve each to the sibling file. Returns
/// `(name, abs_path)` pairs in declaration order.
///
/// Only fires for `<dir>/mod.rs` entry points — flat
/// `tests/<name>.rs` files have no siblings to mirror at the
/// aggregator crate root, since stock cargo doesn't expose any
/// `crate::*` modules for them either.
///
/// Detection is line-based, not syn-driven, so the patterns
/// recognised are intentionally narrow:
///
///   `mod foo;`            (private)
///   `pub mod foo;`        (re-exported)
///   `pub(crate) mod foo;` (crate-visible)
///   `pub(super) mod foo;` (parent-visible).
///
/// `cfg`-gated `#[cfg(...)] mod foo;` is matched only when the cfg
/// attribute is on its own line above `mod foo;` (the cfg is dropped
/// from the re-export — the helper is unconditionally available at
/// the aggregator root, which is the right semantics: cfg-gated
/// helpers were already opt-in at the test-binary level, and the
/// aggregator's `--cfg rudzio_test` activation broadens that gate).
fn discover_test_root_helpers(test_file: &Path) -> Vec<(String, PathBuf)> {
    // Accept `mod.rs`, `main.rs`, and `lib.rs` as test-root entries.
    // Pre-Bug-#7 only `mod.rs` was supported; real workspaces use
    // `main.rs`-rooted (Bug #8C) and `lib.rs`-rooted shapes too,
    // and helper-mod discovery must apply identically to all three.
    let file_name = test_file.file_name().and_then(|os_name| os_name.to_str());
    if !matches!(file_name, Some("mod.rs" | "main.rs" | "lib.rs")) {
        return Vec::new();
    }
    let Some(dir) = test_file.parent() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(test_file) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for raw_line in text.lines() {
        let Some(name) = parse_top_level_mod_decl(raw_line) else {
            continue;
        };
        let sibling_file = dir.join(format!("{name}.rs"));
        let sibling_dir_mod = dir.join(&name).join("mod.rs");
        let resolved = if sibling_file.is_file() {
            sibling_file
        } else if sibling_dir_mod.is_file() {
            sibling_dir_mod
        } else {
            // `mod foo;` with no matching file — either a typo in the
            // member's source or the file is generated at build.rs
            // time. Either way the re-export would point at a missing
            // file and break the aggregator build, so skip it.
            continue;
        };
        out.push((name, resolved));
    }
    out
}

/// Match a single source line against the `[pub(...) ] mod NAME;`
/// pattern and return `NAME` when present. `None` for any other
/// shape (comments, attributes, inline `mod NAME { ... }` blocks,
/// nested mods inside fns or impls — those don't introduce
/// crate-root submodules under stock cargo either, so the
/// re-export must skip them).
fn parse_top_level_mod_decl(raw_line: &str) -> Option<String> {
    let trimmed = raw_line.trim();
    if trimmed.starts_with("//") {
        return None;
    }
    let after_vis = trimmed
        .strip_prefix("pub(crate) ")
        .or_else(|| trimmed.strip_prefix("pub(super) "))
        .or_else(|| trimmed.strip_prefix("pub "))
        .unwrap_or(trimmed);
    let rest = after_vis.strip_prefix("mod ")?;
    let semi_idx = rest.find(';')?;
    let brace_idx = rest.find('{');
    if let Some(brace) = brace_idx
        && brace < semi_idx
    {
        // `mod NAME { ... }` — inline module, no sibling file.
        return None;
    }
    let name = rest.get(..semi_idx)?.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return None;
    }
    Some(name.to_owned())
}

/// Clone `dep` with `optional` cleared.
///
/// Used at the aggregator's emit site. A member declaring
/// `foo = { ..., optional = true }` + a `[features] X = ["dep:foo"]`
/// gate relies on its own `[features]` table to activate the dep.
/// The aggregator merges every member's deps into a single
/// `[dependencies]` table but has no analogous `[features]` table —
/// so re-emitting `optional = true` verbatim leaves the dep gated by
/// nothing, cargo never compiles it, and `#[path]`-included test
/// files referencing the dep fail with `unresolved import`. The
/// aggregator's contract is "every referenced dep must compile";
/// force-enable matches that contract.
fn force_enable(dep: &DevDepSpec) -> DevDepSpec {
    let mut out = dep.clone();
    out.optional = false;
    out
}

/// Build the aggregator crate's `Cargo.toml` from the resolved `Plan`.
fn build_cargo_toml(plan: &Plan) -> Result<String> {
    let mut doc = DocumentMut::new();

    let mut pkg = Table::new();
    pkg.insert("name", value(AGGREGATOR_NAME));
    pkg.insert("version", value("0.0.0"));
    pkg.insert("edition", value("2024"));
    pkg.insert("publish", value(false));
    doc.insert("package", Item::Table(pkg));

    // `[workspace]` makes the aggregator its own workspace root so
    // cargo doesn't complain that its manifest is missing from the
    // enclosing rudzio workspace's `members` list (the aggregator lives
    // at `<target-dir>/rudzio-auto-runner`, outside that list). It also
    // insulates feature unification from the parent graph. The bin-only
    // workspace-member problem that cuts off `rudzio::build::expose_bins`
    // here is handled in `build.rs` — see `build_build_rs`.
    //
    // Bridge crates live at `members/<name>/Cargo.toml`. Cargo would
    // otherwise reject those as "non-member manifests nested inside a
    // workspace root". Listing them as explicit workspace members makes
    // cargo treat them as part of this workspace. If no member
    // qualifies for bridging, the `members` key is omitted entirely so
    // the aggregator stays a single-crate virtual workspace.
    let mut ws_tbl = Table::new();
    let mut members_arr = Array::new();
    for member in &plan.members {
        if !bridge_applies_to(member) {
            continue;
        }
        members_arr.push(Value::String(Formatted::new(format!(
            "members/{}",
            bridge_dir_name(member)
        ))));
    }
    if !members_arr.is_empty() {
        ws_tbl.insert("members", Item::Value(Value::Array(members_arr)));
    }
    doc.insert("workspace", Item::Table(ws_tbl));

    let mut bin = Table::new();
    bin.insert("name", value(AGGREGATOR_NAME));
    bin.insert("path", value("src/main.rs"));
    bin.insert("test", value(false));
    let mut bins = toml_edit::ArrayOfTables::new();
    bins.push(bin);
    doc.insert("bin", Item::ArrayOfTables(bins));

    let mut deps = Table::new();

    // The aggregator injects rudzio (directly, with the union of
    // requested runtime features). anyhow is NOT injected — the
    // migrator emits `::rudzio::BoxError` rather than `::anyhow::Result`
    // for void-fn rewrites, and user-written tests that use anyhow
    // declare it in their own `[dev-dependencies]`; those surface here
    // through the dev-dep union below.
    //
    // rudzio: derived from how workspace members declare it (path, git,
    // or version — never hardcoded to the workspace root, which is wrong
    // for downstream users whose workspace root is NOT rudzio). When
    // rudzio is itself a bridged workspace member (rudzio's own repo
    // dogfooding), route through that member's bridge so the aggregator
    // and the bridges link to a single rudzio rlib — otherwise the
    // linkme `#[distributed_slice]` machinery rudzio uses for test
    // discovery panics with `duplicate #[distributed_slice]` at startup
    // because two distinct rudzio compile units each register the
    // collection.
    deps.insert(
        "rudzio",
        Item::Value(Value::InlineTable(aggregator_rudzio_inline_table(plan))),
    );

    // Every rudzio-using member as a path dep with default features.
    // Skip the workspace root itself (already injected as
    // `rudzio = { path = ..., features = [...] }`) and bin-only members
    // (cargo rejects libless path deps; they participate via the
    // aggregator's `[workspace.members]` list so `expose_bins` can see
    // them in cargo-metadata output).
    //
    // Members that carry src-embedded rudzio suites go through a
    // generated bridge crate (see `bridge_applies_to`): instead of the
    // aggregator depending on the member's real manifest dir (which
    // only has `[dev-dependencies]` visible when cargo compiles the
    // member's own `[[test]]` target — NOT when the aggregator pulls
    // it in as a plain lib), the aggregator depends on the bridge at
    // `./members/<name>/` with `package = "<name>_rudzio_bridge"`. The
    // bridge re-points `[lib] path` at the real `src/lib.rs` but owns
    // the deps cargo sees, so `use ::rudzio::...` inside the member's
    // src tests compiles under `--cfg rudzio_test` without the member's
    // own Cargo.toml carrying any rudzio-specific machinery.
    let workspace_root_abs = plan.workspace_root.as_std_path();
    for member in &plan.members {
        if paths_equal(&member.manifest_dir, workspace_root_abs) {
            continue;
        }
        if !member.has_lib {
            continue;
        }
        let mut tbl = InlineTable::new();
        if bridge_applies_to(member) {
            tbl.insert(
                "path",
                Value::String(Formatted::new(format!(
                    "./members/{}",
                    bridge_dir_name(member)
                ))),
            );
            tbl.insert(
                "package",
                Value::String(Formatted::new(bridge_package_name(member))),
            );
        } else {
            tbl.insert(
                "path",
                Value::String(Formatted::new(
                    member.manifest_dir.to_string_lossy().into_owned(),
                )),
            );
        }
        deps.insert(&member.package_name, Item::Value(Value::InlineTable(tbl)));
    }

    // Union of deps (normal + dev) across all members. First spec
    // encountered wins on path/version/rename; features accumulate
    // across all appearances. Skip rudzio (injected above), the
    // aggregator itself, and intra-workspace sibling members (added
    // separately as path deps).
    let member_names: BTreeSet<String> = plan
        .members
        .iter()
        .map(|member| member.package_name.clone())
        .collect();
    let mut merged: BTreeMap<String, DevDepSpec> = BTreeMap::new();
    for member in &plan.members {
        for dep in &member.dev_deps {
            let entry_name = dep.rename.as_deref().unwrap_or(&dep.name).to_owned();
            if entry_name == RUDZIO_DEP || entry_name == AGGREGATOR_NAME {
                continue;
            }
            if member_names.contains(&entry_name) {
                continue;
            }
            merged
                .entry(entry_name)
                .and_modify(|existing| {
                    for feat in &dep.features {
                        if !existing.features.contains(feat) {
                            existing.features.push(feat.clone());
                        }
                    }
                    existing.uses_default_features &= dep.uses_default_features;
                })
                .or_insert_with(|| dep.clone());
        }
    }
    for (entry_name, dep) in &merged {
        let item = render_dev_dep(&force_enable(dep), plan)?;
        deps.insert(entry_name, item);
    }

    doc.insert("dependencies", Item::Table(deps));

    // No `[build-dependencies]` needed — the aggregator's `build.rs`
    // shells out to `cargo build --bins` directly (std-only) rather than
    // going through `rudzio::build::expose_bins`, which can't reach
    // bin-only workspace members when the aggregator is its own workspace.

    emit_features_table(&mut doc, plan);

    Ok(doc.to_string())
}

/// Emit the aggregator's `[features]` table as the union of every
/// member's `[features]` table so `cargo rudzio test --features X`
/// works the same as `cargo test --features X` would in the consumer
/// (Bug #2). Two members declaring the same feature name produce one
/// entry whose activation list is the union of both.
///
/// `dep:foo` activations are STRIPPED at the aggregator level:
/// Bug #9's force-enable already pulls every optional dep in
/// unconditionally, so the feature no longer needs to gate
/// dep activation. Worse, keeping `dep:foo` would make the
/// aggregator's Cargo.toml invalid because the dep is no
/// longer optional in the aggregator's `[dependencies]` table
/// ("feature includes `dep:foo`, but `foo` is not an optional
/// dependency"). A feature with no surviving activations is
/// emitted as `name = []` — still reachable via `--features X`,
/// just doesn't activate anything (the deps are already on).
fn emit_features_table(doc: &mut DocumentMut, plan: &Plan) {
    let mut features_table: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for member in &plan.members {
        for (name, activations) in &member.features {
            let entry = features_table.entry(name.clone()).or_default();
            for activation in activations {
                if activation.starts_with("dep:") {
                    continue;
                }
                let _newly_inserted: bool = entry.insert(activation.clone());
            }
        }
    }
    if features_table.is_empty() {
        return;
    }
    let mut feats = Table::new();
    for (name, activations) in features_table {
        let mut arr = Array::new();
        for activation in activations {
            arr.push(activation);
        }
        feats.insert(&name, Item::Value(Value::Array(arr)));
    }
    doc.insert("features", Item::Table(feats));
}

/// Render `dep` as a sibling-bridge dependency entry when its package
/// name matches another bridged member of `plan`.
///
/// Without this redirection, the bridge's `[dependencies]` would point
/// at the original member path/version while the aggregator's
/// `[dependencies]` points at the sibling's bridge — cargo would then
/// compile the same crate twice (different package IDs because the
/// paths differ) and the aggregator would drown in
/// `the trait bound X: Y is not satisfied` errors at link time.
///
/// Matches against `member.package_name` (cargo's `[package].name`) so
/// renamed deps (`alias = { package = "real-pkg", ... }`) still resolve
/// to the right sibling — `dep.name` is already the post-rename real
/// package name (see `apply_dev_dep_field`'s `package` branch).
/// Features and `default-features` propagate using the same merge rules
/// as the workspace-inherited / direct branches in `render_dev_dep`, so
/// feature flags reach the bridge's mirrored `[features]` table.
fn render_sibling_bridge_dep(dep: &DevDepSpec, plan: &Plan) -> Option<Item> {
    let sibling = plan
        .members
        .iter()
        .find(|member| member.package_name == dep.name && bridge_applies_to(member))?;

    let mut tbl = InlineTable::new();
    tbl.insert(
        "path",
        Value::String(Formatted::new(format!("../{}", bridge_dir_name(sibling)))),
    );
    tbl.insert(
        "package",
        Value::String(Formatted::new(bridge_package_name(sibling))),
    );

    let ws_entry = if dep.workspace_inherited {
        plan.workspace_deps.get(&dep.name)
    } else {
        None
    };
    let mut feats: Vec<String> = ws_entry.map(|ws| ws.features.clone()).unwrap_or_default();
    feats.extend(dep.features.iter().cloned());
    feats.sort();
    feats.dedup();
    if !feats.is_empty() {
        let mut arr = Array::new();
        for feat in feats {
            arr.push(Value::String(Formatted::new(feat)));
        }
        tbl.insert("features", Value::Array(arr));
    }

    let uses_defaults =
        ws_entry.is_none_or(|ws| ws.uses_default_features) && dep.uses_default_features;
    if !uses_defaults {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }

    if dep.optional {
        tbl.insert("optional", Value::Boolean(Formatted::new(true)));
    }

    Some(Item::Value(Value::InlineTable(tbl)))
}

/// Render a single `DevDepSpec` as a TOML inline table (or bare version string for trivial entries).
fn render_dev_dep(dep: &DevDepSpec, plan: &Plan) -> Result<Item> {
    if let Some(item) = render_sibling_bridge_dep(dep, plan) {
        return Ok(item);
    }
    let registry = resolve_dev_dep_registry(dep, plan);
    let mut tbl = InlineTable::new();
    if dep.workspace_inherited {
        fill_workspace_inherited_body(&mut tbl, dep, plan)?;
    } else if let Some(path_buf) = &dep.path {
        fill_path_dep_body(&mut tbl, dep, path_buf);
    } else if dep.features.is_empty()
        && dep.uses_default_features
        && dep.rename.is_none()
        && !dep.optional
        && registry.is_none()
    {
        return Ok(Item::Value(Value::String(Formatted::new(
            dep.version_req.clone(),
        ))));
    } else {
        fill_version_dep_body(&mut tbl, dep);
    }
    if dep.rename.is_some() {
        tbl.insert("package", Value::String(Formatted::new(dep.name.clone())));
    }
    if dep.optional {
        tbl.insert("optional", Value::Boolean(Formatted::new(true)));
    }
    if let Some(name) = &registry {
        tbl.insert("registry", Value::String(Formatted::new(name.clone())));
    }
    Ok(Item::Value(Value::InlineTable(tbl)))
}

/// Resolve the effective `registry = "<name>"` for a dev-dep entry.
///
/// - Workspace-inherited: member-level `registry` overrides the
///   workspace entry's (cargo's own merge rule for the same key).
/// - Direct: the member's own `registry` is the only source.
///
/// Path-based and git-based locations conceptually can't carry a
/// registry — cargo rejects `registry` alongside `path`/`git` — but
/// we still preserve and emit the field when set so manifests that
/// happen to carry it round-trip unchanged.
fn resolve_dev_dep_registry(dep: &DevDepSpec, plan: &Plan) -> Option<String> {
    if dep.workspace_inherited {
        let ws_registry = plan
            .workspace_deps
            .get(&dep.name)
            .and_then(|ws| ws.registry.clone());
        dep.registry.clone().or(ws_registry)
    } else {
        dep.registry.clone()
    }
}

/// Populate `tbl` with the inline-table body for a workspace-inherited
/// dev-dep entry, expanding the `workspace = true` flag into the
/// concrete location keys (`path`, `version`, or `git` + refs) the
/// aggregator's own (empty) `[workspace]` stanza couldn't resolve.
fn fill_workspace_inherited_body(
    tbl: &mut InlineTable,
    dep: &DevDepSpec,
    plan: &Plan,
) -> Result<()> {
    let ws = plan.workspace_deps.get(&dep.name).ok_or_else(|| {
        anyhow!(
            "dev-dep `{}` says `workspace = true` but root Cargo.toml has no `[workspace.dependencies.{}]` entry",
            dep.name,
            dep.name
        )
    })?;
    if let Some(path_buf) = &ws.path {
        tbl.insert(
            "path",
            Value::String(Formatted::new(path_buf.to_string_lossy().into_owned())),
        );
    } else if let Some(version) = &ws.version_req {
        tbl.insert("version", Value::String(Formatted::new(version.clone())));
    } else if let Some(url) = &ws.git {
        tbl.insert("git", Value::String(Formatted::new(url.clone())));
        match &ws.git_ref {
            Some(GitRef::Rev(rev)) => {
                tbl.insert("rev", Value::String(Formatted::new(rev.clone())));
            }
            Some(GitRef::Branch(branch)) => {
                tbl.insert("branch", Value::String(Formatted::new(branch.clone())));
            }
            Some(GitRef::Tag(tag)) => {
                tbl.insert("tag", Value::String(Formatted::new(tag.clone())));
            }
            None => {}
        }
    } else {
        bail!(
            "workspace dep `{}` has neither `path`, `version`, nor `git` to inherit",
            dep.name
        );
    }
    let mut feats: Vec<String> = ws.features.clone();
    feats.extend(dep.features.iter().cloned());
    feats.sort();
    feats.dedup();
    if !feats.is_empty() {
        let mut arr = Array::new();
        for feat in feats {
            arr.push(Value::String(Formatted::new(feat)));
        }
        tbl.insert("features", Value::Array(arr));
    }
    let dflt = dep.uses_default_features && ws.uses_default_features;
    if !dflt {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }
    Ok(())
}

/// Populate `tbl` with the inline-table body for a direct path-based
/// dev-dep entry — the simplest shape: just `path = "..."` plus any
/// non-default `features` / `default-features = false`.
fn fill_path_dep_body(tbl: &mut InlineTable, dep: &DevDepSpec, path_buf: &Path) {
    tbl.insert(
        "path",
        Value::String(Formatted::new(path_buf.to_string_lossy().into_owned())),
    );
    if !dep.features.is_empty() {
        let mut arr = Array::new();
        for feat in &dep.features {
            arr.push(Value::String(Formatted::new(feat.clone())));
        }
        tbl.insert("features", Value::Array(arr));
    }
    if !dep.uses_default_features {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }
}

/// Populate `tbl` with the inline-table body for a direct version-based
/// dev-dep entry — `version = "..."` plus any non-default `features` /
/// `default-features = false`. Used when the entry is non-trivial
/// enough (features / rename / optional / registry) that the bare
/// version-string shape won't do.
fn fill_version_dep_body(tbl: &mut InlineTable, dep: &DevDepSpec) {
    tbl.insert(
        "version",
        Value::String(Formatted::new(dep.version_req.clone())),
    );
    if !dep.features.is_empty() {
        let mut arr = Array::new();
        for feat in &dep.features {
            arr.push(Value::String(Formatted::new(feat.clone())));
        }
        tbl.insert("features", Value::Array(arr));
    }
    if !dep.uses_default_features {
        tbl.insert("default-features", Value::Boolean(Formatted::new(false)));
    }
}

/// Build the aggregator's `src/tests.rs` body — `#[path]`-included submodules per member's tests.
fn build_tests_rs(plan: &Plan, out_dir: &Path) -> String {
    // Per-crate submodule namespacing. Each member's tests/*.rs files
    // nest under `mod <crate_name> { ... }` so sibling helper modules
    // resolve via `use super::helper::*` from inside a test file —
    // that path works BOTH in per-crate `cargo test -p X` (where the
    // test binary's crate root has `mod helper;` and `mod test_file;`
    // as peers, so super from inside test_file = crate root = has
    // helper) AND in the aggregator (super from tests::X::test_file =
    // tests::X = has mod helper). Flat prefix-mangling (the previous
    // layout) broke the former because `crate::helper` resolved to
    // the aggregator root, which doesn't have helper under its short
    // name. Per-crate nesting also eliminates cross-crate name
    // collisions without needing a prefix at all.
    let proxies = collect_test_binary_proxies(plan, out_dir);
    let mut per_crate_mod_names: BTreeSet<String> = BTreeSet::new();
    let mut out = String::new();
    for member in &plan.members {
        if member.test_files.is_empty() {
            continue;
        }
        let base_ident = sanitize_ident(&member.package_name);
        let mut crate_mod = base_ident.clone();
        let mut dedup_counter = 1_u32;
        while !per_crate_mod_names.insert(crate_mod.clone()) {
            crate_mod = format!("{base_ident}_{dedup_counter}");
            dedup_counter = dedup_counter.saturating_add(1);
        }
        let _crate_header: FmtResult = writeln!(out, "mod {crate_mod} {{");

        let mut used_inner: BTreeSet<String> = BTreeSet::new();
        for file in &member.test_files {
            let stem_ident = inner_module_ident_for(file);
            let mut inner = stem_ident.clone();
            let mut inner_counter = 1_u32;
            while !used_inner.insert(inner.clone()) {
                inner = format!("{stem_ident}_{inner_counter}");
                inner_counter = inner_counter.saturating_add(1);
            }
            // Reference the PROXY test-root, not the original source.
            // The proxy has `mod <helper>;` declarations stripped (so
            // helpers don't compile twice) and `crate::<helper>` paths
            // rewritten to the synth names emitted at the aggregator's
            // crate root. The proxy is materialised by
            // `write_test_binary_proxies` before this string is
            // written to disk.
            let path_for_attr = proxies
                .iter()
                .find(|proxy| proxy.test_root == *file)
                .map_or_else(
                    || file.to_string_lossy().into_owned(),
                    |proxy| proxy.proxy_test_root.to_string_lossy().into_owned(),
                );
            let _path_line: FmtResult = writeln!(
                out,
                "    #[path = {}]\n    mod {inner};",
                quote_str(&path_for_attr)
            );
        }
        out.push_str("}\n");
    }
    out
}

/// Build the aggregator's `build.rs` content — emits `cargo:rustc-cfg=rudzio_test`,
/// per-member manifest-dir env vars, and bin-export shims.
#[inline]
#[must_use]
pub fn build_build_rs(plan: &Plan) -> String {
    let bin_members: Vec<&MemberPlan> = plan
        .members
        .iter()
        .filter(|member| !member.bin_names.is_empty())
        .collect();
    let test_members: Vec<&MemberPlan> = plan
        .members
        .iter()
        .filter(|member| !member.test_files.is_empty())
        .collect();
    // The aggregator always emits `cargo:rustc-cfg=rudzio_test`: this is
    // where the cfg enters the aggregator's own compile unit. Cargo
    // scopes `cargo:rustc-cfg=` per-crate, so the emission here does NOT
    // leak into nested `cargo build --bins` invocations that
    // `expose_member_bins` may trigger below — each of those gets its
    // own untouched rustc.
    //
    // For each member that contributes integration-test sources (and
    // therefore appears under a `mod <member>` block in the
    // aggregator's `tests.rs`), emit
    // `cargo:rustc-env=RUDZIO_MEMBER_MANIFEST_DIR_<sanitised>=<abs>` so
    // the aggregator's `main.rs` can read these via `env!(...)` and
    // wire them into `rudzio::member_meta::register_member_manifest_dirs`.
    // `rudzio::manifest_dir!()` then resolves `module_path!()` at the
    // test's call site to the original member's manifest dir instead
    // of the aggregator's.
    //
    // For bin members, shell out to `cargo build --bins --manifest-path
    // <Cargo.toml>` into a sandboxed OUT_DIR target, then emit
    // `cargo:rustc-env=CARGO_BIN_EXE_<bin>=<abs>` for each so that
    // `env!(CARGO_BIN_EXE_<name>)` in `#[path]`-included integration
    // sources resolves at compile time. (The aggregator has its own
    // `[workspace]` stanza, so `rudzio::build::expose_bins`'s
    // metadata-based approach isn't applicable here.)
    let mut out = String::new();
    if bin_members.is_empty() {
        out.push_str("fn main() {\n");
        out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
        out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
        emit_member_manifest_dir_env_vars(&mut out, &test_members);
        out.push_str("}\n");
        return out;
    }
    out.push_str(BUILD_RS_HELPERS);
    out.push_str("\nfn main() -> Result<(), String> {\n");
    out.push_str("    println!(\"cargo:rustc-cfg=rudzio_test\");\n");
    out.push_str("    println!(\"cargo::rustc-check-cfg=cfg(rudzio_test)\");\n");
    emit_member_manifest_dir_env_vars(&mut out, &test_members);
    for member in bin_members {
        let _ignored: FmtResult = write!(
            out,
            "    expose_member_bins({}, {}, &[",
            quote_str(&member.package_name),
            quote_str(&member.manifest_dir.to_string_lossy()),
        );
        for (idx, bin) in member.bin_names.iter().enumerate() {
            if idx > 0 {
                out.push_str(", ");
            }
            out.push_str(&quote_str(bin));
        }
        out.push_str("])?;\n");
    }
    out.push_str("    Ok(())\n}\n");
    out
}

/// Emit a `println!(...)` line per member that exports the member's
/// manifest dir as a `cargo:rustc-env=RUDZIO_MEMBER_MANIFEST_DIR_<n>=<abs>`
/// directive.
fn emit_member_manifest_dir_env_vars(out: &mut String, members: &[&MemberPlan]) {
    for member in members {
        let ident = sanitize_ident(&member.package_name);
        let _ignored: FmtResult = writeln!(
            out,
            "    println!(\"cargo:rustc-env=RUDZIO_MEMBER_MANIFEST_DIR_{ident}={}\");",
            escape_str_literal(&member.manifest_dir.to_string_lossy())
        );
    }
}

/// Escape `value` for safe inclusion inside a Rust `&str` literal whose
/// surrounding quotes the caller already wrote. Mirrors `quote_str`'s
/// inner-character handling but skips the leading/trailing quotes so
/// the result can be spliced directly into a literal that's part of a
/// larger string.
fn escape_str_literal(value: &str) -> String {
    let mut buf = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => buf.push_str("\\\\"),
            '"' => buf.push_str("\\\""),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            '\0' => buf.push_str("\\0"),
            other if u32::from(other) < 0x20 || other == '\u{7f}' => {
                let _ignored: FmtResult = write!(buf, "\\u{{{:x}}}", u32::from(other));
            }
            other => buf.push(other),
        }
    }
    buf
}

/// Pick the Rust module identifier for a `#[path]`-included test
/// file in the aggregator's `src/tests.rs`.
///
/// Naive `file.file_stem()` breaks on two filenames:
///
/// * `tests/<name>/mod.rs` — stem is literally `mod`, a Rust reserved
///   keyword. Emitting `mod mod;` halts compilation with
///   `expected identifier, found keyword "mod"`. The idiomatic
///   replacement is the parent directory's name (cargo's own
///   convention: a `mod.rs` file IS the parent dir as a module).
///
/// * Any other reserved keyword as the stem (`type.rs`, `fn.rs`,
///   `pub.rs`, …). These can be promoted to raw identifiers
///   (`r#type`) which Rust accepts everywhere including module
///   declarations.
///
/// All other stems pass through [`sanitize_ident`] unchanged.
/// Return the `[[test]]` binary name cargo would assign to `file`.
///
/// Same convention as `inner_module_ident_for` for selecting the
/// human-readable name but WITHOUT sanitisation / reserved-keyword
/// escaping — cargo's `[[test]] name = "<...>"` accepts arbitrary
/// strings, so the test-binary filter compares against the literal
/// directory / stem name the user wrote.
///
/// Used by [`Plan::restrict_to_test_binaries`] to match `--test foo`
/// against the workspace's `[[test]]` entries.
fn test_binary_name_for(file: &Path) -> String {
    let stem = file
        .file_stem()
        .and_then(|os_stem| os_stem.to_str())
        .unwrap_or("test");
    if matches!(stem, "mod" | "main" | "lib") {
        file.parent()
            .and_then(|parent| parent.file_name())
            .and_then(|os_name| os_name.to_str())
            .unwrap_or(stem)
            .to_owned()
    } else {
        stem.to_owned()
    }
}

/// Derive the inner Rust module identifier emitted into the
/// aggregator's proxy `mod.rs` for `file`. Uses the immediate parent
/// directory's name for `mod.rs` entries, the file stem otherwise.
/// Sanitizes invalid identifier characters and promotes Rust reserved
/// keywords to raw identifiers (`r#match`).
fn inner_module_ident_for(file: &Path) -> String {
    let raw_stem = file
        .file_stem()
        .and_then(|os_stem| os_stem.to_str())
        .unwrap_or("test");
    let base = if raw_stem == "mod" {
        // `tests/<parent>/mod.rs` → use `<parent>` as the module name.
        // Fall back to "test_mod" only when the file has no parent at
        // all (defensive — cargo always provides an absolute path).
        let parent_dir = file
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|os_name| os_name.to_str())
            .unwrap_or("test_mod");
        sanitize_ident(parent_dir)
    } else {
        sanitize_ident(raw_stem)
    };
    if is_rust_reserved_keyword(&base) {
        // Promote to a raw identifier so the keyword is usable in a
        // module-declaration position. Applies uniformly whether the
        // keyword came from a file stem (`tests/match.rs` →
        // `r#match`) or a parent dir of a `mod.rs` (`tests/impl/mod.rs`
        // → `r#impl`).
        return format!("r#{base}");
    }
    base
}

/// `true` when `ident` is a Rust reserved keyword (Edition 2021 + 2024
/// committed set). Used to decide whether to promote a generated
/// module name to a raw identifier. Conservative: any name on this
/// list will be emitted as `r#<name>` regardless of context.
///
/// Source: rustc's `kw` symbol table (the union of all editions'
/// reserved sets — pinning to one edition would silently miss a
/// future-edition word).
fn is_rust_reserved_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "do"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "macro"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "priv"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "unsafe"
            | "unsized"
            | "use"
            | "virtual"
            | "where"
            | "while"
            | "yield"
    )
}

/// Replace any non `[a-zA-Z0-9_]` chars in `value` with `_`, prefixing a digit-starter with `_`.
fn sanitize_ident(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit())
    {
        out.insert(0, '_');
    }
    out
}

/// Render `value` as a Rust-source `&str` literal (with surrounding double-quotes and escapes).
///
/// Used by emitters of generated Rust source to embed dynamic strings
/// safely into the output.
fn quote_str(value: &str) -> String {
    let mut buf = String::with_capacity(value.len().saturating_add(2));
    buf.push('"');
    for character in value.chars() {
        match character {
            '\\' => buf.push_str("\\\\"),
            '"' => buf.push_str("\\\""),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            '\0' => buf.push_str("\\0"),
            other if u32::from(other) < 0x20 || other == '\u{7f}' => {
                let _ignored: FmtResult = write!(buf, "\\u{{{:x}}}", u32::from(other));
            }
            other => buf.push(other),
        }
    }
    buf.push('"');
    buf
}

/// Two paths refer to the same canonicalized location.
///
/// Falls back to direct equality when canonicalization fails (the paths
/// may not exist on disk yet — e.g. synthetic test fixtures).
fn paths_equal(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .ok()
        .zip(fs::canonicalize(right).ok())
        .is_some_and(|(canonical_left, canonical_right)| canonical_left == canonical_right)
        || left == right
}

/// Build a `Plan` by querying `cargo metadata` from the current working directory.
///
/// # Errors
///
/// Returns an error when `cargo metadata` fails or when reconciling
/// member metadata into a `Plan` fails.
#[inline]
pub fn plan_from_cwd() -> Result<Plan> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("failed to run `cargo metadata --no-deps` from the current directory")?;
    build_plan(&metadata)
}

/// Build a `Plan` by querying `cargo metadata --manifest-path PATH`.
///
/// Used when the user invokes `cargo rudzio test --manifest-path PATH`
/// to operate on a workspace other than the current directory's.
///
/// # Errors
///
/// Returns an error when `cargo metadata` fails or when reconciling
/// member metadata into a `Plan` fails.
#[inline]
pub fn plan_from_manifest(manifest_path: &Path) -> Result<Plan> {
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_path)
        .no_deps()
        .exec()
        .with_context(|| {
            format!(
                "failed to run `cargo metadata --no-deps --manifest-path {}`",
                manifest_path.display(),
            )
        })?;
    build_plan(&metadata)
}
