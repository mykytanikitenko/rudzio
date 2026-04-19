use common_context::Test;
use rudzio::runtime::compio::Runtime as CompioRuntime;

#[rudzio::suite([
    (
        runtime = CompioRuntime::new,
        global_context = common_context::Global,
        test_context = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    fn passes_under_compio(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
