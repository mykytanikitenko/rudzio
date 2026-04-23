pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
#[cfg(any(test, rudzio_test))]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::Multithread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
mod tests {
    use super::*;
    #[::rudzio::test]
    async fn sums_correctly() -> ::anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
