pub fn answer() -> i32 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn runs_on_multi_thread() {
        assert_eq!(answer(), 42);
    }
}
