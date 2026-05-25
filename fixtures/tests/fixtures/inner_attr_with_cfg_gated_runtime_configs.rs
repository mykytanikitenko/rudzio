//! Bug #15 defensive: inner attributes inside the mod body work
//! correctly when the suite list contains `#[cfg(...)]`-gated runtime
//! configs. The cfg-gated entries exercise a different codegen branch
//! in `expand_suite`; the inner-attr partition must remain correct
//! across that branch.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    (
        runtime = rudzio::runtime::tokio::CurrentThread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    #![allow(clippy::pedantic)]

    use super::Test;

    #[rudzio::test]
    fn inner_attr_survives_cfg_gated_runtime_configs(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
