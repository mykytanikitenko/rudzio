pub async fn hello() -> &'static str {
    "world"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[async_std::test]
    async fn greets() {
        assert_eq!(hello().await, "world");
    }
}
