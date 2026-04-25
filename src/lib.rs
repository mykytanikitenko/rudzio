#[cfg(feature = "build")]
pub mod build;
#[cfg(feature = "common")]
pub mod common;
pub mod config;
pub mod context;
pub mod runner;
pub mod runtime;
pub mod suite;
pub mod test_case;
pub mod token;

pub use rudzio_macro::{main, suite, test};

pub use config::{CargoMeta, ColorMode, Config, Format, RunIgnoredMode};

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
        $crate::CargoMeta {
            manifest_dir: ::std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            pkg_name: env!("CARGO_PKG_NAME").to_owned(),
            pkg_version: env!("CARGO_PKG_VERSION").to_owned(),
            crate_name: env!("CARGO_CRATE_NAME").to_owned(),
        }
    };
}
pub use context::{Suite, Test};
pub use runner::{TestSummary, run};
pub use runtime::{JoinError, Runtime};
pub use suite::{
    RuntimeGroupKey, RuntimeGroupOwner, SuiteId, SuiteReporter, SuiteRunRequest, SuiteSummary,
    TestOutcome, TestRunFn, fnv1a64,
};
pub use test_case::{BoxError, TestCase, TestFn, box_error};
pub use token::{TEST_TOKENS, TestToken};

pub use futures_util;
#[doc(hidden)]
pub use linkme;
#[doc(hidden)]
pub use tokio_util;
