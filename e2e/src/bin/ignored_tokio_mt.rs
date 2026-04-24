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
