pub mod bench;
pub mod bin;
#[cfg(feature = "build")]
pub mod build;
pub mod common;
pub mod config;
pub mod context;
#[doc(hidden)]
pub mod member_meta;
pub mod output;
pub mod parallelism;
pub mod runner;
pub mod runtime;
pub mod suite;
pub mod test_case;
pub mod token;

/// Captures `CARGO_MANIFEST_DIR` as rustc saw it when compiling
/// rudzio's `src/lib.rs`, via the `env!` macro (the rustc-tracked
/// compile-time env channel).
#[doc(hidden)]
pub const __BRIDGE_OBSERVED_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Captures `CARGO_MANIFEST_DIR` as proc-macros saw it via
/// `std::env::var` at expansion time of rudzio's `src/lib.rs`.
///
/// `cargo:rustc-env=CARGO_MANIFEST_DIR=<override>` from a bridge
/// `build.rs` reaches `env!` (the channel above), but cargo may pass
/// reserved env vars to rustc through a separate mechanism that
/// proc-macros' `std::env::var` doesn't see. Third-party
/// proc-macros like `refinery::embed_migrations!` and
/// `sqlx::migrate!` resolve their path arguments via `std::env::var`,
/// not `env!`, so this const exists to verify both channels carry
/// the same override.
#[doc(hidden)]
pub const __BRIDGE_PROC_MACRO_OBSERVED_MANIFEST_DIR: &str =
    __proc_macro_env!("CARGO_MANIFEST_DIR");

pub use bench::{Report, Strategy};
pub use config::{BenchMode, CargoMeta, ColorMode, Config, Format, OutputMode, RunIgnoredMode};
pub use context::{Suite, Test};
pub use futures_util;
#[doc(hidden)]
pub use linkme;
pub use rudzio_macro::{__proc_macro_env, main, suite, test};
pub use runner::{
    TestSummary, normalize_module_path, qualified_test_name, run, token_passes_filters,
};
pub use runtime::{JoinError, Runtime};
pub use suite::{
    Id as SuiteId, Reporter as SuiteReporter, RunRequest as SuiteRunRequest,
    RuntimeGroupKey, RuntimeGroupOwner, Summary as SuiteSummary, TestOutcome, TestRunFn, fnv1a64,
};
pub use test_case::{BoxError, IntoRudzioResult, TestCase, TestFn, box_error};
/// Re-export of the `tokio` runtime crate. Available whenever any of
/// the `runtime-tokio-*` cargo features is on; lets downstream tests
/// reach `tokio::time::sleep` etc. without listing tokio as a separate
/// dev-dep (and crucially, without breaking the `cargo-rudzio`
/// aggregator, which would otherwise see `::tokio` as missing from the
/// generated bridge crate's resolver).
#[doc(hidden)]
#[cfg(any(
    feature = "runtime-tokio-multi-thread",
    feature = "runtime-tokio-current-thread",
    feature = "runtime-tokio-local",
))]
pub use tokio;
#[doc(hidden)]
pub use tokio_util;
pub use token::{TEST_TOKENS, Token as TestToken};

/// Resolve a `[[bin]]` target's executable path at a test call site,
/// returning a [`PathBuf`](std::path::PathBuf).
///
/// Drop-in replacement for `PathBuf::from(env!("CARGO_BIN_EXE_<name>"))`
/// that also works in the two layouts cargo doesn't populate
/// `CARGO_BIN_EXE_*` for on its own:
///
/// - **Shared runner** — an aggregator crate hosting tests that spawn
///   another crate's bins. Requires [`build::expose_bins`] in the
///   aggregator's `build.rs`.
/// - **`cargo test --lib`** — `#[cfg(test)] #[rudzio::main]` in
///   `src/lib.rs`. Either add [`build::expose_self_bins`] to the
///   crate's `build.rs`, or pre-build with `cargo build --bins`.
///
/// Resolution chain:
///
/// 1. `option_env!(concat!("CARGO_BIN_EXE_", <name>))` at compile time.
/// 2. Runtime walk from `std::env::current_exe()` to
///    `target/<profile>/<name>` if step 1 missed.
/// 3. Panic with an actionable message (which fix to apply) if both
///    miss.
///
/// ```rust,ignore
/// let mut child = std::process::Command::new(rudzio::bin!("my-server"))
///     .arg("--port=0")
///     .spawn()?;
/// ```
///
/// The argument must be a string literal (the bin's Cargo target name).
#[macro_export]
macro_rules! bin {
    ($name:literal) => {{
        match ::core::option_env!(::core::concat!("CARGO_BIN_EXE_", $name)) {
            ::core::option::Option::Some(path) => ::std::path::PathBuf::from(path),
            ::core::option::Option::None => match $crate::bin::__resolve_at_runtime($name) {
                ::core::result::Result::Ok(path) => path,
                ::core::result::Result::Err(err) => ::core::panic!("{err}"),
            },
        }
    }};
}

/// Expand to a [`CargoMeta`] populated from the caller crate's
/// `env!(...)` values. Use this when you need to build a [`Config`]
/// outside `#[rudzio::main]` — for example in a unit test:
///
/// ```rust,ignore
/// let config = rudzio::Config::parse(rudzio::cargo_meta!());
/// ```
#[macro_export]
macro_rules! cargo_meta {
    () => {
        $crate::CargoMeta::new(
            env!("CARGO_CRATE_NAME").to_owned(),
            ::std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            env!("CARGO_PKG_NAME").to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        )
    };
}

/// Resolve the original member's `CARGO_MANIFEST_DIR` even when the
/// caller is `#[path]`-included into the cargo-rudzio aggregator.
///
/// Stock `cargo test -p <member>` builds a per-crate test binary whose
/// `env!("CARGO_MANIFEST_DIR")` points at the member — fine. Under
/// `cargo rudzio test` the same source is included into the aggregator
/// crate, where `env!("CARGO_MANIFEST_DIR")` instead returns
/// `<target-dir>/rudzio-auto-runner/`. Harnesses that locate fixtures
/// relative to the member's dir then break.
///
/// Returns the per-member dir from the rudzio runtime registry the
/// aggregator populates at startup; falls back to
/// `env!("CARGO_MANIFEST_DIR")` when the registry is empty (per-crate
/// test runs) or when `module_path!()` isn't aggregator-shaped (any
/// caller outside `#[path]`-included member tests).
///
/// ```rust,ignore
/// let fixtures = rudzio::manifest_dir!().join("fixtures");
/// ```
#[macro_export]
macro_rules! manifest_dir {
    () => {
        $crate::member_meta::resolve_member_manifest_dir(
            ::std::module_path!(),
            env!("CARGO_MANIFEST_DIR"),
        )
    };
}
