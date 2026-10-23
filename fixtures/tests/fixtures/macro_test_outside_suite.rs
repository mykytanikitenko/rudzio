//! A `macro_rules!`-generated `#[rudzio::test]` outside any `#[rudzio::suite]`
//! must fail to compile. The `#[rudzio::test]` attribute re-registers each
//! concrete fn through the hidden per-suite `__rudzio_register_test!` macro,
//! which is only emitted *inside* a suite module — outside one the macro is
//! undefined and rustc reports it.

use rudzio::common::context::Test;

macro_rules! make_test {
    ($name:ident) => {
        #[rudzio::test]
        fn $name(_ctx: &Test) -> anyhow::Result<()> {
            Ok(())
        }
    };
}

make_test!(outside_suite);

#[rudzio::main]
fn main() {}
