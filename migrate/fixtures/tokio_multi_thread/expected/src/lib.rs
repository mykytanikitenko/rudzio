pub fn answer() -> i32 {
    42
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
    use super::*;
    /* pre-migration (rudzio-migrate):
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn runs_on_multi_thread() {
        assert_eq!(answer(), 42);
    }
    */
    #[::rudzio::test]
    async fn runs_on_multi_thread() -> ::anyhow::Result<()> {
        assert_eq!(answer(), 42);
        ::core::result::Result::Ok(())
    }
}
#[cfg(test)]
#[::rudzio::main]
fn main() {}
