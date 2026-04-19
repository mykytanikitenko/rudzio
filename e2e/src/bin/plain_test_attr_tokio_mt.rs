//! The macro recognises both `#[rudzio::test]` and the plain `#[test]`
//! attribute — this bin uses the latter, guarding `is_test_attr` against
//! regressions that accidentally drop the bare form.

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

    #[test]
    fn runs_via_plain_test_attribute(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
