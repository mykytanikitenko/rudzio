// Ignored test bodies use `panic!` to guard against accidental execution —
// if they ever run, the run fails loudly. One of them also uses a bare
// `#[ignore]` on purpose (the fixture exercises exactly that form).
#![allow(
    clippy::panic,
    reason = "ignored tests panic as a tripwire if accidentally executed"
)]
#![allow(
    clippy::ignore_without_reason,
    reason = "fixture exercises the bare `#[ignore]` form on purpose"
)]

use common_context::Test;
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn runs(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    #[ignore]
    fn ignored_bare(_ctx: &Test) -> anyhow::Result<()> {
        panic!("must not run")
    }

    #[rudzio::test]
    #[ignore = "takes too long"]
    fn ignored_with_reason(_ctx: &Test) -> anyhow::Result<()> {
        panic!("must not run")
    }
}

#[rudzio::main]
fn main() {}
