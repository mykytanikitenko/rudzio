pub mod bench;
pub mod bin;
pub mod bridge_meta;
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
pub mod shuffle;
pub mod suite;
pub mod test_case;
pub mod token;

pub use bench::{Report, Strategy};
#[doc(hidden)]
pub use bridge_meta::{__BRIDGE_OBSERVED_MANIFEST_DIR, __BRIDGE_PROC_MACRO_OBSERVED_MANIFEST_DIR};
pub use config::{
    BenchMode, CargoMeta, ColorMode, Config, EnsureTimeConfig, EnsureTimeViolation, Format,
    OutputMode, RunIgnoredMode,
};
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
    Id as SuiteId, Reporter as SuiteReporter, RunRequest as SuiteRunRequest, RuntimeGroupKey,
    RuntimeGroupOwner, Summary as SuiteSummary, TestOutcome, TestRunFn, fnv1a64,
};
pub use test_case::{BoxError, IntoRudzioResult, TestCase, TestFn, box_error};
pub use token::{TEST_TOKENS, Token as TestToken};
/// Re-export of the `async-std` runtime crate. Available whenever the
/// `runtime-async-std` cargo feature is on; lets downstream tests reach
/// `async_std::*` items without listing async-std as a separate dev-dep
/// (mirrors the rationale behind the `tokio` re-export below).
#[doc(hidden)]
#[cfg(feature = "runtime-async-std")]
pub use async_std;
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
