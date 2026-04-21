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
