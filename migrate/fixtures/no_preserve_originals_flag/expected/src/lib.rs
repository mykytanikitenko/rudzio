pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
#[cfg(test)]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
mod tests {
    use ::rudzio::common::context::Test;
    use super::*;
    #[::rudzio::test]
    async fn sums_correctly(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        ::core::result::Result::Ok(())
    }
}
