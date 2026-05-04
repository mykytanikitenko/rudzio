//! Compile-level assertions for the rudzio macro, driven by `trybuild`.

use rudzio::common::context::Suite;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
use rudzio::runtime::{compio, embassy};

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
])]
mod tests {
    use rudzio::common::context::Test;

    #[rudzio::test]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "this fixture drives trybuild compile-pass assertions; the framework requires the test fn signature to return anyhow::Result<()>"
    )]
    fn sync_test_should_compile(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/sync_test.rs");
        Ok(())
    }
}
