//! Bug #5 edge case: the same `macro_rules!` invocation inside two suites in
//! the same file, each with a DIFFERENT runtime. The `shared_test` name is
//! identical in both suites, so a cross-registration bug (one suite's
//! forwarding macro baking in the other suite's config) would register the
//! test four times (once per `(suite, config)` cross-product). The correct
//! behaviour is exactly one registration per suite config: two total runs,
//! one under `tokio::Multithread`, one under `tokio::CurrentThread`.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::{CurrentThread, Multithread};

/// Generate a trivially-passing `#[rudzio::test]` fn, shared verbatim by the
/// two suites below so the same name is generated in each.
macro_rules! make_shared {
    ($name:ident) => {
        #[rudzio::test]
        async fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod mt {
    use super::Test;

    make_shared!(shared_test);
}

#[rudzio::suite([
    (
        runtime = CurrentThread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod ct {
    use super::Test;

    make_shared!(shared_test);
}

#[rudzio::main]
fn main() {}
