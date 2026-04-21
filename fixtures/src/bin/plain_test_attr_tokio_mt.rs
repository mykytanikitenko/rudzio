//! The macro recognises both `#[rudzio::test]` and the plain `#[test]`
//! attribute — this bin uses the latter, guarding `is_test_attr` against
//! regressions that accidentally drop the bare form.

use rudzio::common::context::Test;

#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
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
