pub fn answer() -> i32 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn runs_under_compio() {
        assert_eq!(answer(), 42);
    }
}
