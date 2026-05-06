//! Display-side `Duration` formatting tests for `rudzio::common::time::fmt_duration`.

use std::time::Duration;

use rudzio::common::context::{Suite, Test};
use rudzio::common::time::fmt_duration;
use rudzio::runtime::futures::ThreadPool;
use rudzio::runtime::tokio::{CurrentThread, Local, Multithread};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rudzio::runtime::monoio;
use rudzio::runtime::{async_std, compio, embassy, smol};

#[rudzio::suite([
    (runtime = Multithread::new, suite = Suite, test = Test),
    (runtime = CurrentThread::new, suite = Suite, test = Test),
    (runtime = Local::new, suite = Suite, test = Test),
    (runtime = compio::Runtime::new, suite = Suite, test = Test),
    (runtime = embassy::Runtime::new, suite = Suite, test = Test),
    (runtime = ThreadPool::new, suite = Suite, test = Test),
    (runtime = async_std::Runtime::new, suite = Suite, test = Test),
    (runtime = smol::Runtime::new, suite = Suite, test = Test),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (runtime = monoio::Runtime::new, suite = Suite, test = Test),
])]
mod fmt_duration_unit {
    use super::{Duration, Test, fmt_duration};

    #[rudzio::test]
    async fn formats_seconds(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(fmt_duration(Duration::from_millis(1_230)) == "1.23s");
        anyhow::ensure!(fmt_duration(Duration::from_secs(42)) == "42.00s");
        Ok(())
    }

    #[rudzio::test]
    async fn formats_milliseconds(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(fmt_duration(Duration::from_micros(45_670)) == "45.67ms");
        anyhow::ensure!(fmt_duration(Duration::from_millis(1)) == "1.00ms");
        Ok(())
    }

    #[rudzio::test]
    async fn formats_microseconds(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(fmt_duration(Duration::from_nanos(123_450)) == "123.45\u{b5}s");
        anyhow::ensure!(fmt_duration(Duration::from_micros(1)) == "1.00\u{b5}s");
        Ok(())
    }

    #[rudzio::test]
    async fn formats_nanoseconds(_ctx: &Test) -> anyhow::Result<()> {
        anyhow::ensure!(fmt_duration(Duration::from_nanos(42)) == "42.00ns");
        anyhow::ensure!(fmt_duration(Duration::from_nanos(0)) == "0.00ns");
        Ok(())
    }
}
