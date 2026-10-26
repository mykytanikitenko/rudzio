//! Raw-identifier test names (`fn r#type`, `fn r#match`) must register and
//! run without the suite codegen panicking. The per-test hidden idents
//! (`__RUDZIO_TOKEN_{MOD}_{TEST}_{cfg_idx}` and `__rudzio_run_test_{MOD}_{TEST}_{cfg_idx}`)
//! are built with `format_ident!` over the test ident's string form, which
//! for a raw ident carries the `r#` prefix — an invalid identifier fragment.
//! The codegen strips that prefix so the generated names stay well-formed.

use rudzio::common::context::{Suite, Test};
use rudzio::runtime::tokio::Multithread;

#[rudzio::suite([
    (
        runtime = Multithread::new,
        suite = Suite,
        test = Test,
    ),
])]
mod tests {
    use super::Test;

    #[rudzio::test]
    async fn r#type(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    async fn r#match(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }

    #[rudzio::test]
    async fn ordinary_name(_ctx: &Test) -> anyhow::Result<()> {
        Ok(())
    }
}

#[rudzio::main]
fn main() {}
