//! Compile-level assertions for the rudzio macro, driven by `trybuild`.


#[rudzio::suite([
    (
        runtime = rudzio::runtime::tokio::Multithread::new,
        suite = rudzio::common::context::Suite,
        test = rudzio::common::context::Test,
    ),
])]
mod tests {
    use rudzio::common::context::Test;

    #[rudzio::test]
    fn sync_test_should_compile(_ctx: &Test) -> anyhow::Result<()> {
        let cases = trybuild::TestCases::new();
        cases.pass("tests/fixtures/sync_test.rs");
        Ok(())
    }
}
