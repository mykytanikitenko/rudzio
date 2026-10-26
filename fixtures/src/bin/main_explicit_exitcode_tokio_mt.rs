//! `#[rudzio::main]` on a fn with an explicit return type
//! (`fn main() -> ::std::process::ExitCode`). The macro discards the user's
//! declared signature and always emits `fn main() -> ExitCode` wrapping the
//! body, so an explicit return-type annotation must be tolerated without
//! changing behaviour.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn passes_under_explicit_exitcode(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() -> ::std::process::ExitCode {}
