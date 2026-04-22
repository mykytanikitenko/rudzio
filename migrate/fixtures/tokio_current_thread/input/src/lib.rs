pub fn answer() -> i32 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn runs_on_current_thread() {
        assert_eq!(answer(), 42);
    }
}
