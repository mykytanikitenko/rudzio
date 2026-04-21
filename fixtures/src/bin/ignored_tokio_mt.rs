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
