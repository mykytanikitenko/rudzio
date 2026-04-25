//! Reproducer: mod tests carries extra outer attrs between
//! `#[cfg(test)]` and the `mod` keyword. The migrator must place
//! `#[rudzio::suite(...)]` between cfg(test) and the `mod` keyword
//! (or immediately before `mod`) so that cfg(test) gates the macro
//! expansion and any later attrs still apply to the expanded code.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
#[cfg(test)]
#[expect(clippy::panic_in_result_fn, reason = "assertions panic by design")]
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
    /* pre-migration (rudzio-migrate):
    #[test]
    fn sums_correctly() {
        assert_eq!(add(1, 2), 3);
    }
    */
    #[::rudzio::test]
    async fn sums_correctly() -> ::anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
