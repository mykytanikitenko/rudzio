pub fn answer() -> i32 {
    42
}
#[cfg(any(test, rudzio_test))]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::CurrentThread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
mod tests {
    use super::*;
    /* pre-migration (rudzio-migrate):
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn runs_on_current_thread() {
        assert_eq!(answer(), 42);
    }
    */
    #[::rudzio::test]
    async fn runs_on_current_thread() -> ::anyhow::Result<()> {
        assert_eq!(answer(), 42);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
