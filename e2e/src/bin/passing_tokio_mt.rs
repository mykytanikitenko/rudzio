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
    fn first_passes(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    fn second_passes(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
