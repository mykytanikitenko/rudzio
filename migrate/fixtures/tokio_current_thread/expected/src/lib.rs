pub fn answer() -> i32 {
    42
}
#[cfg(test)]
#[::rudzio::suite(
    [(
        runtime = ::rudzio::runtime::tokio::CurrentThread::new,
        suite = ::rudzio::common::context::Suite,
        test = ::rudzio::common::context::Test,
    ),
    ]
)]
mod tests {
    use ::rudzio::common::context::Test;
    use super::*;
    /* pre-migration (rudzio-migrate):
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn runs_on_current_thread() {
        assert_eq!(answer(), 42);
    }
    */
    #[::rudzio::test]
    async fn runs_on_current_thread(_ctx: &Test) -> ::anyhow::Result<()> {
        assert_eq!(answer(), 42);
        ::core::result::Result::Ok(())
    }
}
