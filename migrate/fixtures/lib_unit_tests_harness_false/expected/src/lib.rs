//! Reproducer: src/ unit tests need `[lib] harness = false` in
//! Cargo.toml plus `#[cfg(test)] #[rudzio::main] fn main() {}` at
//! the bottom of lib.rs, otherwise libtest remains in control and
//! rudzio's converted tests never run.
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
    /* pre-migration (rudzio-migrate):
    #[test]
    fn sums_correctly() {
        assert_eq!(add(1, 2), 3);
    }
    */
    #[::rudzio::test]
    async fn sums_correctly(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(add(1, 2), 3);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
