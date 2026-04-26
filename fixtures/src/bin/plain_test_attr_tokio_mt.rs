//! The macro recognises both `#[rudzio::test]` and the plain `#[test]`
//! attribute — this bin uses the latter, guarding `is_test_attr` against
//! regressions that accidentally drop the bare form.

use rudzio::common::context::Suite;
use rudzio::common::context::Test;
use rudzio::runtime::tokio::Multithread;

#[expect(
    clippy::unnecessary_wraps,
    reason = "this fixture exercises the bare #[test] attribute on a body that trivially passes; the framework requires the test fn signature to return anyhow::Result<()>"
)]
#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    #[test]
    fn runs_via_plain_test_attribute(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
