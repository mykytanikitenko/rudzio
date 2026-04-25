pub fn product(a: i32, b: i32) -> i32 {
    a * b
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
mod outer {
    use ::rudzio::common::context::Test;
    use super::*;
    mod inner {
        use super::*;
        /* pre-migration (rudzio-migrate):
        #[test]
        fn inner_multiplies() {
            assert_eq!(product(3, 4), 12);
        }
        */
        #[::rudzio::test]
        async fn inner_multiplies(_ctx: &Test) -> ::anyhow::Result<()> {
            assert_eq!(product(3, 4), 12);
            ::core::result::Result::Ok(())
        }
    }
    /* pre-migration (rudzio-migrate):
    #[test]
    fn outer_multiplies() {
        assert_eq!(product(5, 6), 30);
    }
    */
    #[::rudzio::test]
    async fn outer_multiplies(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(product(5, 6), 30);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
