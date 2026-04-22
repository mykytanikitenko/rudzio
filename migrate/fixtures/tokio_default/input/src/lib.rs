pub async fn delay() {
    tokio::task::yield_now().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delays_cleanly() {
        delay().await;
    }
}
